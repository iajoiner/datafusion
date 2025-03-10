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

//! [`JsonFormat`]: Line delimited JSON [`FileFormat`] abstractions

use std::any::Any;
use std::collections::HashMap;
use std::fmt;
use std::fmt::Debug;
use std::io::BufReader;
use std::sync::Arc;

use super::write::orchestration::spawn_writer_tasks_and_join;
use super::{
    Decoder, DecoderDeserializer, FileFormat, FileFormatFactory, FileScanConfig,
    DEFAULT_SCHEMA_INFER_MAX_RECORD,
};
use crate::datasource::file_format::file_compression_type::FileCompressionType;
use crate::datasource::file_format::write::demux::DemuxedStreamReceiver;
use crate::datasource::file_format::write::BatchSerializer;
use crate::datasource::physical_plan::{FileSink, FileSinkConfig, JsonSource};
use crate::error::Result;
use crate::execution::SessionState;
use crate::physical_plan::insert::{DataSink, DataSinkExec};
use crate::physical_plan::{
    DisplayAs, DisplayFormatType, SendableRecordBatchStream, Statistics,
};

use arrow::array::RecordBatch;
use arrow::datatypes::{Schema, SchemaRef};
use arrow::error::ArrowError;
use arrow::json;
use arrow::json::reader::{infer_json_schema_from_iterator, ValueIter};
use datafusion_catalog::Session;
use datafusion_common::config::{ConfigField, ConfigFileType, JsonOptions};
use datafusion_common::file_options::json_writer::JsonWriterOptions;
use datafusion_common::{not_impl_err, GetExt, DEFAULT_JSON_EXTENSION};
use datafusion_common_runtime::SpawnedTask;
use datafusion_datasource::display::FileGroupDisplay;
use datafusion_datasource::file::FileSource;
use datafusion_execution::TaskContext;
use datafusion_expr::dml::InsertOp;
use datafusion_physical_expr::PhysicalExpr;
use datafusion_physical_plan::ExecutionPlan;

use async_trait::async_trait;
use bytes::{Buf, Bytes};
use datafusion_physical_expr_common::sort_expr::LexRequirement;
use object_store::{GetResultPayload, ObjectMeta, ObjectStore};

#[derive(Default)]
/// Factory struct used to create [JsonFormat]
pub struct JsonFormatFactory {
    /// the options carried by format factory
    pub options: Option<JsonOptions>,
}

impl JsonFormatFactory {
    /// Creates an instance of [JsonFormatFactory]
    pub fn new() -> Self {
        Self { options: None }
    }

    /// Creates an instance of [JsonFormatFactory] with customized default options
    pub fn new_with_options(options: JsonOptions) -> Self {
        Self {
            options: Some(options),
        }
    }
}

impl FileFormatFactory for JsonFormatFactory {
    fn create(
        &self,
        state: &dyn Session,
        format_options: &HashMap<String, String>,
    ) -> Result<Arc<dyn FileFormat>> {
        let state = state.as_any().downcast_ref::<SessionState>().unwrap();
        let json_options = match &self.options {
            None => {
                let mut table_options = state.default_table_options();
                table_options.set_config_format(ConfigFileType::JSON);
                table_options.alter_with_string_hash_map(format_options)?;
                table_options.json
            }
            Some(json_options) => {
                let mut json_options = json_options.clone();
                for (k, v) in format_options {
                    json_options.set(k, v)?;
                }
                json_options
            }
        };

        Ok(Arc::new(JsonFormat::default().with_options(json_options)))
    }

    fn default(&self) -> Arc<dyn FileFormat> {
        Arc::new(JsonFormat::default())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl GetExt for JsonFormatFactory {
    fn get_ext(&self) -> String {
        // Removes the dot, i.e. ".parquet" -> "parquet"
        DEFAULT_JSON_EXTENSION[1..].to_string()
    }
}

impl Debug for JsonFormatFactory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonFormatFactory")
            .field("options", &self.options)
            .finish()
    }
}

/// New line delimited JSON `FileFormat` implementation.
#[derive(Debug, Default)]
pub struct JsonFormat {
    options: JsonOptions,
}

impl JsonFormat {
    /// Set JSON options
    pub fn with_options(mut self, options: JsonOptions) -> Self {
        self.options = options;
        self
    }

    /// Retrieve JSON options
    pub fn options(&self) -> &JsonOptions {
        &self.options
    }

    /// Set a limit in terms of records to scan to infer the schema
    /// - defaults to `DEFAULT_SCHEMA_INFER_MAX_RECORD`
    pub fn with_schema_infer_max_rec(mut self, max_rec: usize) -> Self {
        self.options.schema_infer_max_rec = Some(max_rec);
        self
    }

    /// Set a `FileCompressionType` of JSON
    /// - defaults to `FileCompressionType::UNCOMPRESSED`
    pub fn with_file_compression_type(
        mut self,
        file_compression_type: FileCompressionType,
    ) -> Self {
        self.options.compression = file_compression_type.into();
        self
    }
}

#[async_trait]
impl FileFormat for JsonFormat {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn get_ext(&self) -> String {
        JsonFormatFactory::new().get_ext()
    }

    fn get_ext_with_compression(
        &self,
        file_compression_type: &FileCompressionType,
    ) -> Result<String> {
        let ext = self.get_ext();
        Ok(format!("{}{}", ext, file_compression_type.get_ext()))
    }

    async fn infer_schema(
        &self,
        _state: &dyn Session,
        store: &Arc<dyn ObjectStore>,
        objects: &[ObjectMeta],
    ) -> Result<SchemaRef> {
        let mut schemas = Vec::new();
        let mut records_to_read = self
            .options
            .schema_infer_max_rec
            .unwrap_or(DEFAULT_SCHEMA_INFER_MAX_RECORD);
        let file_compression_type = FileCompressionType::from(self.options.compression);
        for object in objects {
            let mut take_while = || {
                let should_take = records_to_read > 0;
                if should_take {
                    records_to_read -= 1;
                }
                should_take
            };

            let r = store.as_ref().get(&object.location).await?;
            let schema = match r.payload {
                GetResultPayload::File(file, _) => {
                    let decoder = file_compression_type.convert_read(file)?;
                    let mut reader = BufReader::new(decoder);
                    let iter = ValueIter::new(&mut reader, None);
                    infer_json_schema_from_iterator(iter.take_while(|_| take_while()))?
                }
                GetResultPayload::Stream(_) => {
                    let data = r.bytes().await?;
                    let decoder = file_compression_type.convert_read(data.reader())?;
                    let mut reader = BufReader::new(decoder);
                    let iter = ValueIter::new(&mut reader, None);
                    infer_json_schema_from_iterator(iter.take_while(|_| take_while()))?
                }
            };

            schemas.push(schema);
            if records_to_read == 0 {
                break;
            }
        }

        let schema = Schema::try_merge(schemas)?;
        Ok(Arc::new(schema))
    }

    async fn infer_stats(
        &self,
        _state: &dyn Session,
        _store: &Arc<dyn ObjectStore>,
        table_schema: SchemaRef,
        _object: &ObjectMeta,
    ) -> Result<Statistics> {
        Ok(Statistics::new_unknown(&table_schema))
    }

    async fn create_physical_plan(
        &self,
        _state: &dyn Session,
        mut conf: FileScanConfig,
        _filters: Option<&Arc<dyn PhysicalExpr>>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let source = Arc::new(JsonSource::new());
        conf.file_compression_type = FileCompressionType::from(self.options.compression);
        Ok(conf.with_source(source).build())
    }

    async fn create_writer_physical_plan(
        &self,
        input: Arc<dyn ExecutionPlan>,
        _state: &dyn Session,
        conf: FileSinkConfig,
        order_requirements: Option<LexRequirement>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        if conf.insert_op != InsertOp::Append {
            return not_impl_err!("Overwrites are not implemented yet for Json");
        }

        let writer_options = JsonWriterOptions::try_from(&self.options)?;

        let sink = Arc::new(JsonSink::new(conf, writer_options));

        Ok(Arc::new(DataSinkExec::new(input, sink, order_requirements)) as _)
    }

    fn file_source(&self) -> Arc<dyn FileSource> {
        Arc::new(JsonSource::default())
    }
}

impl Default for JsonSerializer {
    fn default() -> Self {
        Self::new()
    }
}

/// Define a struct for serializing Json records to a stream
pub struct JsonSerializer {}

impl JsonSerializer {
    /// Constructor for the JsonSerializer object
    pub fn new() -> Self {
        Self {}
    }
}

impl BatchSerializer for JsonSerializer {
    fn serialize(&self, batch: RecordBatch, _initial: bool) -> Result<Bytes> {
        let mut buffer = Vec::with_capacity(4096);
        let mut writer = json::LineDelimitedWriter::new(&mut buffer);
        writer.write(&batch)?;
        Ok(Bytes::from(buffer))
    }
}

/// Implements [`DataSink`] for writing to a Json file.
pub struct JsonSink {
    /// Config options for writing data
    config: FileSinkConfig,
    /// Writer options for underlying Json writer
    writer_options: JsonWriterOptions,
}

impl Debug for JsonSink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsonSink").finish()
    }
}

impl DisplayAs for JsonSink {
    fn fmt_as(&self, t: DisplayFormatType, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match t {
            DisplayFormatType::Default | DisplayFormatType::Verbose => {
                write!(f, "JsonSink(file_groups=",)?;
                FileGroupDisplay(&self.config.file_groups).fmt_as(t, f)?;
                write!(f, ")")
            }
        }
    }
}

impl JsonSink {
    /// Create from config.
    pub fn new(config: FileSinkConfig, writer_options: JsonWriterOptions) -> Self {
        Self {
            config,
            writer_options,
        }
    }

    /// Retrieve the writer options
    pub fn writer_options(&self) -> &JsonWriterOptions {
        &self.writer_options
    }
}

#[async_trait]
impl FileSink for JsonSink {
    fn config(&self) -> &FileSinkConfig {
        &self.config
    }

    async fn spawn_writer_tasks_and_join(
        &self,
        context: &Arc<TaskContext>,
        demux_task: SpawnedTask<Result<()>>,
        file_stream_rx: DemuxedStreamReceiver,
        object_store: Arc<dyn ObjectStore>,
    ) -> Result<u64> {
        let serializer = Arc::new(JsonSerializer::new()) as _;
        spawn_writer_tasks_and_join(
            context,
            serializer,
            self.writer_options.compression.into(),
            object_store,
            demux_task,
            file_stream_rx,
        )
        .await
    }
}

#[async_trait]
impl DataSink for JsonSink {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> &SchemaRef {
        self.config.output_schema()
    }

    async fn write_all(
        &self,
        data: SendableRecordBatchStream,
        context: &Arc<TaskContext>,
    ) -> Result<u64> {
        FileSink::write_all(self, data, context).await
    }
}

#[derive(Debug)]
pub(crate) struct JsonDecoder {
    inner: json::reader::Decoder,
}

impl JsonDecoder {
    pub(crate) fn new(decoder: json::reader::Decoder) -> Self {
        Self { inner: decoder }
    }
}

impl Decoder for JsonDecoder {
    fn decode(&mut self, buf: &[u8]) -> Result<usize, ArrowError> {
        self.inner.decode(buf)
    }

    fn flush(&mut self) -> Result<Option<RecordBatch>, ArrowError> {
        self.inner.flush()
    }

    fn can_flush_early(&self) -> bool {
        false
    }
}

impl From<json::reader::Decoder> for DecoderDeserializer<JsonDecoder> {
    fn from(decoder: json::reader::Decoder) -> Self {
        DecoderDeserializer::new(JsonDecoder::new(decoder))
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::scan_format;
    use super::*;
    use crate::datasource::file_format::{
        BatchDeserializer, DecoderDeserializer, DeserializerOutput,
    };
    use crate::execution::options::NdJsonReadOptions;
    use crate::physical_plan::collect;
    use crate::prelude::{SessionConfig, SessionContext};
    use crate::test::object_store::local_unpartitioned_file;

    use arrow::compute::concat_batches;
    use arrow::datatypes::{DataType, Field};
    use arrow::json::ReaderBuilder;
    use arrow::util::pretty;
    use datafusion_common::cast::as_int64_array;
    use datafusion_common::stats::Precision;
    use datafusion_common::{assert_batches_eq, internal_err};

    use futures::StreamExt;
    use object_store::local::LocalFileSystem;
    use regex::Regex;
    use rstest::rstest;

    #[tokio::test]
    async fn read_small_batches() -> Result<()> {
        let config = SessionConfig::new().with_batch_size(2);
        let session_ctx = SessionContext::new_with_config(config);
        let state = session_ctx.state();
        let task_ctx = state.task_ctx();
        let projection = None;
        let exec = get_exec(&state, projection, None).await?;
        let stream = exec.execute(0, task_ctx)?;

        let tt_batches: i32 = stream
            .map(|batch| {
                let batch = batch.unwrap();
                assert_eq!(4, batch.num_columns());
                assert_eq!(2, batch.num_rows());
            })
            .fold(0, |acc, _| async move { acc + 1i32 })
            .await;

        assert_eq!(tt_batches, 6 /* 12/2 */);

        // test metadata
        assert_eq!(exec.statistics()?.num_rows, Precision::Absent);
        assert_eq!(exec.statistics()?.total_byte_size, Precision::Absent);

        Ok(())
    }

    #[tokio::test]
    async fn read_limit() -> Result<()> {
        let session_ctx = SessionContext::new();
        let state = session_ctx.state();
        let task_ctx = state.task_ctx();
        let projection = None;
        let exec = get_exec(&state, projection, Some(1)).await?;
        let batches = collect(exec, task_ctx).await?;
        assert_eq!(1, batches.len());
        assert_eq!(4, batches[0].num_columns());
        assert_eq!(1, batches[0].num_rows());

        Ok(())
    }

    #[tokio::test]
    async fn infer_schema() -> Result<()> {
        let projection = None;
        let session_ctx = SessionContext::new();
        let state = session_ctx.state();
        let exec = get_exec(&state, projection, None).await?;

        let x: Vec<String> = exec
            .schema()
            .fields()
            .iter()
            .map(|f| format!("{}: {:?}", f.name(), f.data_type()))
            .collect();
        assert_eq!(vec!["a: Int64", "b: Float64", "c: Boolean", "d: Utf8",], x);

        Ok(())
    }

    #[tokio::test]
    async fn read_int_column() -> Result<()> {
        let session_ctx = SessionContext::new();
        let state = session_ctx.state();
        let task_ctx = state.task_ctx();
        let projection = Some(vec![0]);
        let exec = get_exec(&state, projection, None).await?;

        let batches = collect(exec, task_ctx).await.expect("Collect batches");

        assert_eq!(1, batches.len());
        assert_eq!(1, batches[0].num_columns());
        assert_eq!(12, batches[0].num_rows());

        let array = as_int64_array(batches[0].column(0))?;
        let mut values: Vec<i64> = vec![];
        for i in 0..batches[0].num_rows() {
            values.push(array.value(i));
        }

        assert_eq!(
            vec![1, -10, 2, 1, 7, 1, 1, 5, 1, 1, 1, 100000000000000],
            values
        );

        Ok(())
    }

    async fn get_exec(
        state: &dyn Session,
        projection: Option<Vec<usize>>,
        limit: Option<usize>,
    ) -> Result<Arc<dyn ExecutionPlan>> {
        let filename = "tests/data/2.json";
        let format = JsonFormat::default();
        scan_format(state, &format, ".", filename, projection, limit).await
    }

    #[tokio::test]
    async fn infer_schema_with_limit() {
        let session = SessionContext::new();
        let ctx = session.state();
        let store = Arc::new(LocalFileSystem::new()) as _;
        let filename = "tests/data/schema_infer_limit.json";
        let format = JsonFormat::default().with_schema_infer_max_rec(3);

        let file_schema = format
            .infer_schema(&ctx, &store, &[local_unpartitioned_file(filename)])
            .await
            .expect("Schema inference");

        let fields = file_schema
            .fields()
            .iter()
            .map(|f| format!("{}: {:?}", f.name(), f.data_type()))
            .collect::<Vec<_>>();
        assert_eq!(vec!["a: Int64", "b: Float64", "c: Boolean"], fields);
    }

    async fn count_num_partitions(ctx: &SessionContext, query: &str) -> Result<usize> {
        let result = ctx
            .sql(&format!("EXPLAIN {query}"))
            .await?
            .collect()
            .await?;

        let plan = format!("{}", &pretty::pretty_format_batches(&result)?);

        let re = Regex::new(r"file_groups=\{(\d+) group").unwrap();

        if let Some(captures) = re.captures(&plan) {
            if let Some(match_) = captures.get(1) {
                let count = match_.as_str().parse::<usize>().unwrap();
                return Ok(count);
            }
        }

        internal_err!("Query contains no Exec: file_groups")
    }

    #[rstest(n_partitions, case(1), case(2), case(3), case(4))]
    #[tokio::test]
    async fn it_can_read_ndjson_in_parallel(n_partitions: usize) -> Result<()> {
        let config = SessionConfig::new()
            .with_repartition_file_scans(true)
            .with_repartition_file_min_size(0)
            .with_target_partitions(n_partitions);

        let ctx = SessionContext::new_with_config(config);

        let table_path = "tests/data/1.json";
        let options = NdJsonReadOptions::default();

        ctx.register_json("json_parallel", table_path, options)
            .await?;

        let query = "SELECT sum(a) FROM json_parallel;";

        let result = ctx.sql(query).await?.collect().await?;
        let actual_partitions = count_num_partitions(&ctx, query).await?;

        #[rustfmt::skip]
        let expected = [
            "+----------------------+",
            "| sum(json_parallel.a) |",
            "+----------------------+",
            "| -7                   |",
            "+----------------------+"
        ];

        assert_batches_eq!(expected, &result);
        assert_eq!(n_partitions, actual_partitions);

        Ok(())
    }

    #[tokio::test]
    async fn it_can_read_empty_ndjson() -> Result<()> {
        let config = SessionConfig::new()
            .with_repartition_file_scans(true)
            .with_repartition_file_min_size(0);

        let ctx = SessionContext::new_with_config(config);

        let table_path = "tests/data/empty.json";
        let options = NdJsonReadOptions::default();

        ctx.register_json("json_parallel_empty", table_path, options)
            .await?;

        let query = "SELECT * FROM json_parallel_empty WHERE random() > 0.5;";

        let result = ctx.sql(query).await?.collect().await?;

        #[rustfmt::skip]
        let expected = [
            "++",
            "++",
        ];

        assert_batches_eq!(expected, &result);

        Ok(())
    }

    #[test]
    fn test_json_deserializer_finish() -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("c1", DataType::Int64, true),
            Field::new("c2", DataType::Int64, true),
            Field::new("c3", DataType::Int64, true),
            Field::new("c4", DataType::Int64, true),
            Field::new("c5", DataType::Int64, true),
        ]));
        let mut deserializer = json_deserializer(1, &schema)?;

        deserializer.digest(r#"{ "c1": 1, "c2": 2, "c3": 3, "c4": 4, "c5": 5 }"#.into());
        deserializer.digest(r#"{ "c1": 6, "c2": 7, "c3": 8, "c4": 9, "c5": 10 }"#.into());
        deserializer
            .digest(r#"{ "c1": 11, "c2": 12, "c3": 13, "c4": 14, "c5": 15 }"#.into());
        deserializer.finish();

        let mut all_batches = RecordBatch::new_empty(schema.clone());
        for _ in 0..3 {
            let output = deserializer.next()?;
            let DeserializerOutput::RecordBatch(batch) = output else {
                panic!("Expected RecordBatch, got {:?}", output);
            };
            all_batches = concat_batches(&schema, &[all_batches, batch])?
        }
        assert_eq!(deserializer.next()?, DeserializerOutput::InputExhausted);

        let expected = [
            "+----+----+----+----+----+",
            "| c1 | c2 | c3 | c4 | c5 |",
            "+----+----+----+----+----+",
            "| 1  | 2  | 3  | 4  | 5  |",
            "| 6  | 7  | 8  | 9  | 10 |",
            "| 11 | 12 | 13 | 14 | 15 |",
            "+----+----+----+----+----+",
        ];

        assert_batches_eq!(expected, &[all_batches]);

        Ok(())
    }

    #[test]
    fn test_json_deserializer_no_finish() -> Result<()> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("c1", DataType::Int64, true),
            Field::new("c2", DataType::Int64, true),
            Field::new("c3", DataType::Int64, true),
            Field::new("c4", DataType::Int64, true),
            Field::new("c5", DataType::Int64, true),
        ]));
        let mut deserializer = json_deserializer(1, &schema)?;

        deserializer.digest(r#"{ "c1": 1, "c2": 2, "c3": 3, "c4": 4, "c5": 5 }"#.into());
        deserializer.digest(r#"{ "c1": 6, "c2": 7, "c3": 8, "c4": 9, "c5": 10 }"#.into());
        deserializer
            .digest(r#"{ "c1": 11, "c2": 12, "c3": 13, "c4": 14, "c5": 15 }"#.into());

        let mut all_batches = RecordBatch::new_empty(schema.clone());
        // We get RequiresMoreData after 2 batches because of how json::Decoder works
        for _ in 0..2 {
            let output = deserializer.next()?;
            let DeserializerOutput::RecordBatch(batch) = output else {
                panic!("Expected RecordBatch, got {:?}", output);
            };
            all_batches = concat_batches(&schema, &[all_batches, batch])?
        }
        assert_eq!(deserializer.next()?, DeserializerOutput::RequiresMoreData);

        let expected = [
            "+----+----+----+----+----+",
            "| c1 | c2 | c3 | c4 | c5 |",
            "+----+----+----+----+----+",
            "| 1  | 2  | 3  | 4  | 5  |",
            "| 6  | 7  | 8  | 9  | 10 |",
            "+----+----+----+----+----+",
        ];

        assert_batches_eq!(expected, &[all_batches]);

        Ok(())
    }

    fn json_deserializer(
        batch_size: usize,
        schema: &Arc<Schema>,
    ) -> Result<impl BatchDeserializer<Bytes>> {
        let decoder = ReaderBuilder::new(schema.clone())
            .with_batch_size(batch_size)
            .build_decoder()?;
        Ok(DecoderDeserializer::new(JsonDecoder::new(decoder)))
    }
}
