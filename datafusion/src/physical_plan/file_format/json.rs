// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

//! Execution plan for reading line-delimited JSON files
use crate::datasource::object_store::ReadSeek;
use crate::error::{DataFusionError, Result};
use crate::execution::runtime_env::RuntimeEnv;
use crate::physical_plan::expressions::PhysicalSortExpr;
use crate::physical_plan::{
    DisplayFormatType, ExecutionPlan, Partitioning, SendableRecordBatchStream, Statistics,
};
use arrow::datatypes::SchemaRef;
use arrow::error::Result as ArrowResult;
use arrow::io::ndjson;
use arrow::io::ndjson::read::FallibleStreamingIterator;
use async_trait::async_trait;
use datafusion_common::record_batch::RecordBatch;
use std::any::Any;
use std::io::{BufRead, BufReader};
use std::sync::Arc;

use super::file_stream::{BatchIter, FileStream};
use super::FileScanConfig;

/// Execution plan for scanning NdJson data source
#[derive(Debug, Clone)]
pub struct NdJsonExec {
    base_config: FileScanConfig,
    projected_statistics: Statistics,
    projected_schema: SchemaRef,
}

impl NdJsonExec {
    /// Create a new JSON reader execution plan provided base configurations
    pub fn new(base_config: FileScanConfig) -> Self {
        let (projected_schema, projected_statistics) = base_config.project();

        Self {
            base_config,
            projected_schema,
            projected_statistics,
        }
    }
}

// TODO: implement iterator in upstream json::Reader type
struct JsonBatchReader<R: ReadSeek> {
    reader: R,
    schema: SchemaRef,
    #[allow(dead_code)]
    proj: Option<Vec<String>>,
    rows: Vec<String>,
}

impl<R: ReadSeek> JsonBatchReader<R> {
    fn new(
        reader: R,
        schema: SchemaRef,
        batch_size: usize,
        proj: Option<Vec<String>>,
    ) -> Self {
        Self {
            reader,
            schema,
            proj,
            rows: vec![String::default(); batch_size],
        }
    }
}

impl<R: BufRead + ReadSeek> Iterator for JsonBatchReader<R> {
    type Item = ArrowResult<RecordBatch>;

    fn next(&mut self) -> Option<Self::Item> {
        let data_type = ndjson::read::infer(&mut self.reader, None).ok()?;
        self.reader.rewind().ok()?;

        // json::read::read_rows iterates on the empty vec and reads at most n rows
        let mut reader =
            ndjson::read::FileReader::new(&mut self.reader, self.rows.clone(), None);

        let mut arrays = vec![];
        // `next` is IO-bounded
        while let Some(rows) = reader.next().ok()? {
            // `deserialize` is CPU-bounded
            let array = ndjson::read::deserialize(rows, data_type.clone()).ok()?;
            arrays.push(array);
        }
        Some(RecordBatch::try_new(self.schema.clone(), arrays))
    }
}

#[async_trait]
impl ExecutionPlan for NdJsonExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.projected_schema.clone()
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(self.base_config.file_groups.len())
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        None
    }

    fn relies_on_input_order(&self) -> bool {
        false
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        Vec::new()
    }

    fn with_new_children(
        &self,
        children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if children.is_empty() {
            Ok(Arc::new(self.clone()) as Arc<dyn ExecutionPlan>)
        } else {
            Err(DataFusionError::Internal(format!(
                "Children cannot be replaced in {:?}",
                self
            )))
        }
    }

    async fn execute(
        &self,
        partition: usize,
        runtime: Arc<RuntimeEnv>,
    ) -> Result<SendableRecordBatchStream> {
        let proj = self.base_config.projected_file_column_names();

        let batch_size = runtime.batch_size();
        let file_schema = Arc::clone(&self.base_config.file_schema);

        // The json reader cannot limit the number of records, so `remaining` is ignored.
        let fun = move |file, _remaining: &Option<usize>| {
            Box::new(JsonBatchReader::new(
                BufReader::new(file),
                file_schema.clone(),
                batch_size,
                proj.clone(),
            )) as BatchIter
        };

        Ok(Box::pin(FileStream::new(
            Arc::clone(&self.base_config.object_store),
            self.base_config.file_groups[partition].clone(),
            fun,
            Arc::clone(&self.projected_schema),
            self.base_config.limit,
            self.base_config.table_partition_cols.clone(),
        )))
    }

    fn fmt_as(
        &self,
        t: DisplayFormatType,
        f: &mut std::fmt::Formatter,
    ) -> std::fmt::Result {
        match t {
            DisplayFormatType::Default => {
                write!(
                    f,
                    "JsonExec: limit={:?}, files={}",
                    self.base_config.limit,
                    super::FileGroupsDisplay(&self.base_config.file_groups),
                )
            }
        }
    }

    fn statistics(&self) -> Statistics {
        self.projected_statistics.clone()
    }
}

#[cfg(test)]
mod tests {
    use arrow::array::Array;
    use arrow::datatypes::{Field, Schema};
    use futures::StreamExt;

    use crate::datasource::{
        file_format::{json::JsonFormat, FileFormat},
        object_store::local::{
            local_object_reader_stream, local_unpartitioned_file, LocalFileSystem,
        },
    };
    use datafusion_common::field_util::SchemaExt;

    use super::*;

    const TEST_DATA_BASE: &str = "tests/jsons";

    async fn infer_schema(path: String) -> Result<SchemaRef> {
        JsonFormat::default()
            .infer_schema(local_object_reader_stream(vec![path]))
            .await
    }

    #[tokio::test]
    async fn nd_json_exec_file_without_projection() -> Result<()> {
        let runtime = Arc::new(RuntimeEnv::default());
        use arrow::datatypes::DataType;
        let path = format!("{}/1.json", TEST_DATA_BASE);

        let row_schema = infer_schema(path.clone()).await?;
        let file_schema = Arc::new(Schema::new(vec![Field::new("", DataType::Struct(row_schema.fields.clone()), true)]));

        let exec = NdJsonExec::new(FileScanConfig {
            object_store: Arc::new(LocalFileSystem {}),
            file_groups: vec![vec![local_unpartitioned_file(path)]],
            file_schema,
            statistics: Statistics::default(),
            projection: None,
            limit: Some(3),
            table_partition_cols: vec![],
        });

        let mut it = exec.execute(0, runtime).await?;
        let batch = it.next().await.unwrap()?;

        assert_eq!(batch.num_rows(), 3);
        let values = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::StructArray>()
            .unwrap();
        let field_a = values.values()[0].as_any().downcast_ref::<arrow::array::Int64Array>().unwrap();
        assert_eq!(field_a.value(0), 1);
        assert_eq!(field_a.value(1), -10);
        assert_eq!(field_a.value(2), 2);

        Ok(())
    }

    #[tokio::test]
    async fn nd_json_exec_file_with_missing_column() -> Result<()> {
        let runtime = Arc::new(RuntimeEnv::default());
        use arrow::datatypes::DataType;
        let path = format!("{}/1.json", TEST_DATA_BASE);

        let actual_schema = infer_schema(path.clone()).await?;

        let mut fields = actual_schema.fields().to_vec();
        fields.push(Field::new("missing_col", DataType::Int32, true));
        let missing_field_idx = fields.len() - 1;

        let file_schema = Arc::new(Schema::new(fields));

        let exec = NdJsonExec::new(FileScanConfig {
            object_store: Arc::new(LocalFileSystem {}),
            file_groups: vec![vec![local_unpartitioned_file(path.clone())]],
            file_schema,
            statistics: Statistics::default(),
            projection: None,
            limit: Some(3),
            table_partition_cols: vec![],
        });

        let mut it = exec.execute(0, runtime).await?;
        let batch = it.next().await.unwrap()?;

        assert_eq!(batch.num_rows(), 3);
        let values = batch
            .column(missing_field_idx)
            .as_any()
            .downcast_ref::<arrow::array::Int32Array>()
            .unwrap();
        assert_eq!(values.len(), 3);
        assert!(values.is_null(0));
        assert!(values.is_null(1));
        assert!(values.is_null(2));

        Ok(())
    }

    #[tokio::test]
    async fn nd_json_exec_file_projection() -> Result<()> {
        let runtime = Arc::new(RuntimeEnv::default());
        let path = format!("{}/1.json", TEST_DATA_BASE);
        let exec = NdJsonExec::new(FileScanConfig {
            object_store: Arc::new(LocalFileSystem {}),
            file_groups: vec![vec![local_unpartitioned_file(path.clone())]],
            file_schema: infer_schema(path).await?,
            statistics: Statistics::default(),
            projection: Some(vec![0, 2]),
            limit: None,
            table_partition_cols: vec![],
        });
        let inferred_schema = exec.schema();
        assert_eq!(inferred_schema.fields().len(), 2);

        inferred_schema.field_with_name("a").unwrap();
        inferred_schema.field_with_name("b").unwrap_err();
        inferred_schema.field_with_name("c").unwrap();
        inferred_schema.field_with_name("d").unwrap_err();

        let mut it = exec.execute(0, runtime).await?;
        let batch = it.next().await.unwrap()?;

        assert_eq!(batch.num_rows(), 4);
        let values = batch
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int64Array>()
            .unwrap();
        assert_eq!(values.value(0), 1);
        assert_eq!(values.value(1), -10);
        assert_eq!(values.value(2), 2);
        Ok(())
    }
}
