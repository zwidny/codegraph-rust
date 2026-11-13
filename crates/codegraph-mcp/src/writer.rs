// ABOUTME: Bounded asynchronous graph writer with durable ordering barriers.
// ABOUTME: Separates database batching and queue memory from inference settings.

//! The queue itself is backend-neutral: it holds `Arc<dyn IndexStorage>` handles, so
//! a Postgres pool runs through the same batching, backpressure and flush barriers
//! as SurrealDB.

use anyhow::{Result, anyhow};
use codegraph_core::CodeNode;
use codegraph_graph::{
    ChunkEmbeddingRecord, FileMetadataRecord, NodeEmbeddingRecord, ProjectMetadataRecord,
    SymbolEmbeddingRecord, edge::CodeEdge, storage::IndexStorage,
};
use futures::{StreamExt, stream::FuturesUnordered};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;

enum Job {
    Nodes(Vec<CodeNode>),
    Edges(Vec<CodeEdge>),
    NodeEmbeddings(Vec<NodeEmbeddingRecord>),
    SymbolEmbeddings(Vec<SymbolEmbeddingRecord>),
    ChunkEmbeddings(Vec<ChunkEmbeddingRecord>),
    FileMetadata(Vec<FileMetadataRecord>),
    DeleteFiles { paths: Vec<String>, project: String },
    ProjectMetadata(ProjectMetadataRecord),
    Flush(oneshot::Sender<Result<()>>),
    Shutdown(oneshot::Sender<Result<()>>),
}

struct Envelope {
    job: Job,
    _memory: Option<OwnedSemaphorePermit>,
    bytes: u64,
    /// Set when the item is about to be queued; used to measure queue wait.
    enqueued_at: Instant,
}

#[derive(Clone, Copy)]
struct Limits {
    rows: usize,
    bytes: usize,
    queue_bytes: usize,
}

impl Limits {
    fn from_env() -> Self {
        fn value(name: &str, default: usize) -> usize {
            std::env::var(name)
                .ok()
                .and_then(|s| s.parse().ok())
                .filter(|v| *v > 0)
                .unwrap_or(default)
        }
        let queue_bytes =
            value("CODEGRAPH_WRITE_QUEUE_BYTES", 32 * 1024 * 1024).clamp(1024, u32::MAX as usize);
        Self {
            rows: value("CODEGRAPH_DB_BATCH_ROWS", 512),
            bytes: value("CODEGRAPH_DB_BATCH_BYTES", 4 * 1024 * 1024).min(queue_bytes),
            queue_bytes,
        }
    }
}

struct ByteCounter(usize);
impl std::io::Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 += bytes.len();
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encoded_size(value: &impl Serialize) -> Result<usize> {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

#[derive(Default)]
struct WriteMetrics {
    jobs: AtomicU64,
    rows: AtomicU64,
    bytes: AtomicU64,
}

/// Full (unsampled) write-timing audit, enabled by `CODEGRAPH_TRACE_WRITER=1`.
///
/// Off by default: when no trace is installed, no samples are recorded and no
/// logs are emitted, so the production path is unchanged.
#[derive(Default)]
struct TraceMetrics {
    /// Sum of time blocked waiting for queue capacity or the byte permit.
    backpressure_us: AtomicU64,
    /// Individual backpressure samples, for percentiles.
    backpressure_samples: std::sync::Mutex<Vec<u64>>,
    queue_wait_us: std::sync::Mutex<Vec<u64>>,
    exec_us: std::sync::Mutex<Vec<u64>>,
    by_kind: std::sync::Mutex<BTreeMap<&'static str, KindStats>>,
    inflight: AtomicUsize,
    inflight_max: AtomicUsize,
    exec_total_us: AtomicU64,
    /// Time spent inside `encoded_size` while building batches (client CPU).
    serialize_us: AtomicU64,
    /// Number of rows passed through `encoded_size`.
    serialize_rows: AtomicU64,
    started_at: std::sync::Mutex<Option<Instant>>,
}

#[derive(Default, Clone)]
struct KindStats {
    jobs: u64,
    rows: u64,
    exec_us: Vec<u64>,
}

/// Nearest-rank percentile over an unordered sample. Returns 0 for an empty slice.
fn percentile_us(samples: &mut [u64], p: f64) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let n = samples.len();
    let rank = ((p * n as f64).ceil() as usize).max(1).min(n);
    samples[rank - 1]
}

impl TraceMetrics {
    fn snapshot(&self, jobs: u64, rows: u64) -> String {
        let wall_ms = self
            .started_at
            .lock()
            .unwrap()
            .map_or(0, |started| started.elapsed().as_millis() as u64);
        let wall_us = wall_ms.saturating_mul(1000).max(1);
        // Total backpressure is retained for callers/forensics; percentiles use samples.
        let _backpressure_total = self.backpressure_us.load(Ordering::Relaxed);
        let exec_total = self.exec_total_us.load(Ordering::Relaxed);
        let inflight_max = self.inflight_max.load(Ordering::Relaxed);
        let inflight_avg = exec_total as f64 / wall_us as f64;

        let mut backpressure = self.backpressure_samples.lock().unwrap().clone();
        let mut queue_wait = self.queue_wait_us.lock().unwrap().clone();
        let mut exec = self.exec_us.lock().unwrap().clone();
        let backpressure_max = backpressure.iter().copied().max().unwrap_or(0);
        let queue_wait_max = queue_wait.iter().copied().max().unwrap_or(0);
        let exec_max = exec.iter().copied().max().unwrap_or(0);

        let mut out = format!(
            "WRITER_TRACE jobs={jobs} rows={rows} wall_ms={wall_ms}\n  \
             backpressure_us p50={} p95={} max={}\n  \
             queue_wait_us   p50={} p95={} max={}\n  \
             exec_us         p50={} p95={} p99={} max={}\n  \
             inflight avg={inflight_avg:.2} max={inflight_max}\n  by_kind",
            percentile_us(&mut backpressure, 0.50),
            percentile_us(&mut backpressure, 0.95),
            backpressure_max,
            percentile_us(&mut queue_wait, 0.50),
            percentile_us(&mut queue_wait, 0.95),
            queue_wait_max,
            percentile_us(&mut exec, 0.50),
            percentile_us(&mut exec, 0.95),
            percentile_us(&mut exec, 0.99),
            exec_max,
        );
        let by_kind = self.by_kind.lock().unwrap();
        for (kind, stats) in by_kind.iter() {
            let mut samples = stats.exec_us.clone();
            out.push_str(&format!(
                " {kind}: jobs={} rows={} exec_p50={} exec_p95={}",
                stats.jobs,
                stats.rows,
                percentile_us(&mut samples, 0.50),
                percentile_us(&mut samples, 0.95),
            ));
        }
        let ser_us = self.serialize_us.load(Ordering::Relaxed);
        let ser_rows = self.serialize_rows.load(Ordering::Relaxed);
        let per_row = if ser_rows == 0 { 0 } else { ser_us / ser_rows };
        out.push_str(&format!(
            "\n  encode_size total_us={ser_us} rows={ser_rows} per_row_us={per_row} share={:.1}%",
            if wall_us == 0 {
                0.0
            } else {
                ser_us as f64 * 100.0 / wall_us as f64
            }
        ));
        out
    }
}

impl Job {
    fn kind(&self) -> &'static str {
        match self {
            Self::Nodes(_) => "nodes",
            Self::Edges(_) => "edges",
            Self::NodeEmbeddings(_) => "node_embeddings",
            Self::SymbolEmbeddings(_) => "symbol_embeddings",
            Self::ChunkEmbeddings(_) => "chunks",
            Self::FileMetadata(_) => "file_metadata",
            Self::DeleteFiles { .. } => "delete_files",
            Self::ProjectMetadata(_) => "project_metadata",
            Self::Flush(_) => "flush",
            Self::Shutdown(_) => "shutdown",
        }
    }
    fn rows(&self) -> usize {
        match self {
            Self::Nodes(rows) => rows.len(),
            Self::Edges(rows) => rows.len(),
            Self::NodeEmbeddings(rows) => rows.len(),
            Self::SymbolEmbeddings(rows) => rows.len(),
            Self::ChunkEmbeddings(rows) => rows.len(),
            Self::FileMetadata(rows) => rows.len(),
            Self::ProjectMetadata(_) => 1,
            Self::DeleteFiles { paths, .. } => paths.len(),
            _ => 0,
        }
    }
}
async fn execute(
    job: Job,
    storage: Arc<dyn IndexStorage>,
    metrics: &WriteMetrics,
    bytes: u64,
) -> Result<()> {
    let rows = job.rows();
    match job {
        Job::Nodes(rows) => {
            storage.upsert_nodes(&rows).await?;
        }
        Job::Edges(rows) => {
            storage.upsert_edges(&rows).await?;
        }
        Job::NodeEmbeddings(rows) => {
            storage.update_node_embeddings(&rows).await?;
        }
        Job::SymbolEmbeddings(rows) => {
            storage.upsert_symbols(&rows).await?;
        }
        Job::ChunkEmbeddings(rows) => {
            storage.upsert_chunks(&rows).await?;
        }
        Job::FileMetadata(rows) => {
            storage.upsert_file_metadata(&rows).await?;
        }
        Job::DeleteFiles { paths, project } => {
            storage.delete_data_for_files(&project, &paths).await?;
        }
        Job::ProjectMetadata(row) => storage.upsert_project_metadata(row).await?,
        Job::Flush(_) | Job::Shutdown(_) => unreachable!("barriers handled by writer"),
    }
    metrics.jobs.fetch_add(1, Ordering::Relaxed);
    let previous_rows = metrics.rows.fetch_add(rows as u64, Ordering::Relaxed);
    metrics.bytes.fetch_add(bytes, Ordering::Relaxed);
    // File-visible progress every ~5k rows. The interactive progress bars are dropped
    // when stdout is not a TTY, so a stalled write otherwise looks like silence.
    let total_rows = previous_rows + rows as u64;
    if total_rows / 5_000 != previous_rows / 5_000 {
        tracing::info!("graph writer: {total_rows} rows persisted");
    }
    Ok(())
}

fn record_result(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    error: &mut Option<String>,
) {
    if let Err(err) = result
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
    {
        tracing::error!("Graph write failed: {err}");
        error.get_or_insert_with(|| err.to_string());
    }
}

pub(crate) struct SurrealWriterHandle {
    tx: mpsc::Sender<Envelope>,
    memory: Arc<Semaphore>,
    limits: Limits,
    join: JoinHandle<()>,
    metrics: Arc<WriteMetrics>,
    trace: Option<Arc<TraceMetrics>>,
}

impl SurrealWriterHandle {
    pub(crate) fn new(pool: Vec<Arc<dyn IndexStorage>>) -> Self {
        assert!(!pool.is_empty(), "writer requires a storage handle");
        let limits = Limits::from_env();
        let memory = Arc::new(Semaphore::new(limits.queue_bytes));
        let (tx, mut rx) = mpsc::channel::<Envelope>(8);
        let metrics = Arc::new(WriteMetrics::default());
        let worker_metrics = metrics.clone();
        let trace =
            std::env::var_os("CODEGRAPH_TRACE_WRITER").map(|_| Arc::new(TraceMetrics::default()));
        let worker_trace = trace.clone();
        let join = tokio::spawn(async move {
            let trace = worker_trace;
            let mut running = FuturesUnordered::<JoinHandle<Result<()>>>::new();
            let mut error = None;
            let mut next = 0;
            while let Some(envelope) = rx.recv().await {
                let dequeued = Instant::now();
                if let Some(trace) = &trace {
                    let wait = dequeued.duration_since(envelope.enqueued_at).as_micros() as u64;
                    trace.queue_wait_us.lock().unwrap().push(wait);
                    if let Ok(mut started) = trace.started_at.lock()
                        && started.is_none()
                    {
                        *started = Some(dequeued);
                    }
                }
                let barrier = matches!(
                    envelope.job,
                    Job::DeleteFiles { .. }
                        | Job::FileMetadata(_)
                        | Job::ProjectMetadata(_)
                        | Job::Flush(_)
                        | Job::Shutdown(_)
                );
                if barrier {
                    while let Some(result) = running.next().await {
                        record_result(result, &mut error);
                    }
                    match envelope.job {
                        Job::Flush(response) => {
                            if let Some(trace) = &trace {
                                let jobs = worker_metrics.jobs.load(Ordering::Relaxed);
                                let rows = worker_metrics.rows.load(Ordering::Relaxed);
                                tracing::info!("{}", trace.snapshot(jobs, rows));
                            }
                            let _ = response.send(error.take().map_or(Ok(()), |e| Err(anyhow!(e))));
                        }
                        Job::Shutdown(response) => {
                            let _ = response.send(error.take().map_or(Ok(()), |e| Err(anyhow!(e))));
                            break;
                        }
                        job => {
                            // Completion metadata must never mark a failed write set current.
                            if error.is_none()
                                && let Err(err) =
                                    execute(job, pool[0].clone(), &worker_metrics, envelope.bytes)
                                        .await
                            {
                                error = Some(err.to_string());
                            }
                        }
                    }
                } else {
                    while running.len() >= pool.len() {
                        record_result(running.next().await.expect("in-flight write"), &mut error);
                    }
                    let storage = pool[next % pool.len()].clone();
                    next += 1;
                    let metrics = worker_metrics.clone();
                    let kind = envelope.job.kind();
                    let rows = envelope.job.rows() as u64;
                    if let Some(trace) = &trace {
                        let current = trace.inflight.fetch_add(1, Ordering::Relaxed) + 1;
                        trace.inflight_max.fetch_max(current, Ordering::Relaxed);
                    }
                    let task_trace = trace.clone();
                    running.push(tokio::spawn(async move {
                        let Envelope {
                            job,
                            _memory,
                            bytes,
                            ..
                        } = envelope;
                        let started = Instant::now();
                        let result = execute(job, storage, &metrics, bytes).await;
                        let elapsed = started.elapsed();
                        if let Some(trace) = task_trace {
                            let us = elapsed.as_micros() as u64;
                            trace.exec_us.lock().unwrap().push(us);
                            trace.exec_total_us.fetch_add(us, Ordering::Relaxed);
                            let mut by_kind = trace.by_kind.lock().unwrap();
                            let stats = by_kind.entry(kind).or_default();
                            stats.jobs += 1;
                            stats.rows += rows;
                            stats.exec_us.push(us);
                            drop(by_kind);
                            trace.inflight.fetch_sub(1, Ordering::Relaxed);
                        }
                        drop(_memory);
                        result
                    }));
                }
            }
            // A dropped indexer closes the channel; finish accepted work before exiting.
            while let Some(result) = running.next().await {
                record_result(result, &mut error);
            }
        });
        Self {
            tx,
            memory,
            limits,
            join,
            metrics,
            trace,
        }
    }

    pub(crate) fn metrics(&self) -> (u64, u64, u64) {
        (
            self.metrics.jobs.load(Ordering::Relaxed),
            self.metrics.rows.load(Ordering::Relaxed),
            self.metrics.bytes.load(Ordering::Relaxed),
        )
    }

    async fn send(&self, job: Job, bytes: usize) -> Result<()> {
        if bytes > self.limits.queue_bytes {
            return Err(anyhow!("Write exceeds queue byte budget: {bytes}"));
        }
        let send_started = Instant::now();
        let permit = self
            .memory
            .clone()
            .acquire_many_owned(bytes.max(1) as u32)
            .await?;
        let result = self
            .tx
            .send(Envelope {
                job,
                _memory: Some(permit),
                bytes: bytes as u64,
                enqueued_at: Instant::now(),
            })
            .await
            .map_err(|_| anyhow!("Graph writer unavailable"));
        if let Some(trace) = &self.trace {
            let elapsed = send_started.elapsed().as_micros() as u64;
            trace.backpressure_us.fetch_add(elapsed, Ordering::Relaxed);
            trace.backpressure_samples.lock().unwrap().push(elapsed);
        }
        result
    }

    async fn batch<T: Serialize>(&self, rows: Vec<T>, wrap: impl Fn(Vec<T>) -> Job) -> Result<()> {
        let mut batch = Vec::new();
        let mut bytes = 2usize;
        for row in rows {
            // Count encoded bytes without allocating another serialized batch.
            let serialize_started = Instant::now();
            let size = encoded_size(&row)? + 1;
            if let Some(trace) = &self.trace {
                trace.serialize_us.fetch_add(
                    serialize_started.elapsed().as_micros() as u64,
                    Ordering::Relaxed,
                );
                trace.serialize_rows.fetch_add(1, Ordering::Relaxed);
            }
            if size + 2 > self.limits.bytes {
                return Err(anyhow!(
                    "One graph record needs {} bytes; increase CODEGRAPH_DB_BATCH_BYTES (current {})",
                    size + 2,
                    self.limits.bytes
                ));
            }
            if !batch.is_empty()
                && (batch.len() >= self.limits.rows || bytes + size > self.limits.bytes)
            {
                self.send(wrap(std::mem::take(&mut batch)), bytes).await?;
                bytes = 2;
            }
            bytes += size;
            batch.push(row);
        }
        if !batch.is_empty() {
            self.send(wrap(batch), bytes).await?;
        }
        Ok(())
    }

    pub(crate) async fn enqueue_nodes(&self, rows: Vec<CodeNode>) -> Result<()> {
        self.batch(rows, Job::Nodes).await
    }
    pub(crate) async fn enqueue_edges(&self, rows: Vec<CodeEdge>) -> Result<()> {
        self.batch(rows, Job::Edges).await
    }
    pub(crate) async fn enqueue_node_embeddings(
        &self,
        rows: Vec<NodeEmbeddingRecord>,
    ) -> Result<()> {
        self.batch(rows, Job::NodeEmbeddings).await
    }
    pub(crate) async fn enqueue_symbol_embeddings(
        &self,
        rows: Vec<SymbolEmbeddingRecord>,
    ) -> Result<()> {
        self.batch(rows, Job::SymbolEmbeddings).await
    }
    pub(crate) async fn enqueue_chunk_embeddings(
        &self,
        rows: Vec<ChunkEmbeddingRecord>,
    ) -> Result<()> {
        self.batch(rows, Job::ChunkEmbeddings).await
    }
    pub(crate) async fn enqueue_file_metadata(&self, rows: Vec<FileMetadataRecord>) -> Result<()> {
        self.batch(rows, Job::FileMetadata).await
    }
    pub(crate) async fn enqueue_delete_nodes_by_file(
        &self,
        paths: Vec<String>,
        project: &str,
    ) -> Result<()> {
        self.batch(paths, |paths| Job::DeleteFiles {
            paths,
            project: project.to_string(),
        })
        .await
    }
    pub(crate) async fn enqueue_project_metadata(&self, row: ProjectMetadataRecord) -> Result<()> {
        let bytes = encoded_size(&row)?;
        self.send(Job::ProjectMetadata(row), bytes).await
    }
    pub(crate) async fn flush(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Job::Flush(tx), 1).await?;
        rx.await
            .map_err(|_| anyhow!("Graph writer ended before acknowledgement"))?
    }
    pub(crate) async fn shutdown(self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.send(Job::Shutdown(tx), 1).await?;
        let result = rx
            .await
            .map_err(|_| anyhow!("Graph writer ended before acknowledgement"))?;
        self.join.await?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use codegraph_core::{Language, Location, NodeType};
    use codegraph_graph::{SurrealDbConfig, SurrealDbStorage};

    async fn fixture() -> (SurrealWriterHandle, Arc<SurrealDbStorage>) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = SurrealDbConfig::embedded(dir.path());
        config.connection = "mem://".into();
        config.database = format!(
            "writer_{}",
            dir.path()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .replace('.', "_")
        );
        config.auto_migrate = false;
        let storage = Arc::new(SurrealDbStorage::new(config.clone()).await.unwrap());
        storage.db().query("DEFINE TABLE nodes SCHEMALESS; DEFINE FIELD name ON nodes TYPE string ASSERT $value != 'bad'; DEFINE TABLE project_metadata SCHEMALESS;").await.unwrap().check().unwrap();
        let first: Arc<dyn IndexStorage> = storage.clone();
        let second: Arc<dyn IndexStorage> = storage.clone();
        let mut writer = SurrealWriterHandle::new(vec![first, second]);
        writer.limits.rows = 1;
        writer.limits.bytes = 2048;
        (writer, storage)
    }

    fn node(name: &str) -> CodeNode {
        CodeNode::new(
            name.to_string(),
            Some(NodeType::Function),
            Some(Language::Rust),
            Location {
                file_path: "a.rs".into(),
                line: 1,
                column: 0,
                end_line: None,
                end_column: None,
            },
        )
    }

    fn metadata() -> ProjectMetadataRecord {
        ProjectMetadataRecord {
            metadata: serde_json::json!({}),
            project_id: "test".into(),
            name: "test".into(),
            root_path: "/test".into(),
            primary_language: None,
            file_count: 1,
            node_count: 1,
            edge_count: 0,
            avg_coverage_score: 1.0,
            last_analyzed: chrono::Utc::now(),
            codegraph_version: "test".into(),
            organization_id: None,
            domain: None,
        }
    }

    #[tokio::test]
    async fn failed_writes_do_not_advance_metadata_and_flush_allows_retry() {
        let (writer, storage) = fixture().await;
        writer
            .enqueue_nodes(vec![node("good"), node("bad")])
            .await
            .unwrap();
        writer.enqueue_project_metadata(metadata()).await.unwrap();
        assert!(writer.flush().await.is_err());
        let mut response = storage
            .db()
            .query("SELECT * FROM project_metadata")
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<serde_json::Value> = response.take(0).unwrap();
        assert!(rows.is_empty());
        writer.enqueue_nodes(vec![node("retry")]).await.unwrap();
        writer.enqueue_project_metadata(metadata()).await.unwrap();
        writer.flush().await.unwrap();
        let mut response = storage
            .db()
            .query("SELECT * FROM project_metadata")
            .await
            .unwrap()
            .check()
            .unwrap();
        let rows: Vec<serde_json::Value> = response.take(0).unwrap();
        assert_eq!(rows.len(), 1);
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn oversized_records_fail_before_entering_queue() {
        let (writer, _) = fixture().await;
        let large = node("large").with_content("x".repeat(4096));
        assert!(
            writer
                .enqueue_nodes(vec![large])
                .await
                .unwrap_err()
                .to_string()
                .contains("CODEGRAPH_DB_BATCH_BYTES")
        );
        writer.flush().await.unwrap();
        writer.shutdown().await.unwrap();
    }

    #[test]
    fn percentile_empty_is_zero() {
        let mut samples: Vec<u64> = Vec::new();
        assert_eq!(percentile_us(&mut samples, 0.50), 0);
        assert_eq!(percentile_us(&mut samples, 0.95), 0);
    }

    #[test]
    fn percentile_single_sample() {
        let mut samples = [42u64];
        assert_eq!(percentile_us(&mut samples, 0.50), 42);
        assert_eq!(percentile_us(&mut samples, 0.95), 42);
        assert_eq!(percentile_us(&mut samples, 0.99), 42);
    }

    #[test]
    fn percentile_p50_and_p95_on_known_sample() {
        let mut samples = [1u64, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(percentile_us(&mut samples, 0.50), 5);
        assert_eq!(percentile_us(&mut samples, 0.95), 10);
    }

    #[test]
    fn percentile_is_order_independent() {
        let mut ordered = [1u64, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let mut shuffled = [10u64, 3, 7, 1, 9, 2, 8, 4, 6, 5];
        assert_eq!(
            percentile_us(&mut ordered, 0.50),
            percentile_us(&mut shuffled, 0.50)
        );
        assert_eq!(
            percentile_us(&mut ordered, 0.95),
            percentile_us(&mut shuffled, 0.95)
        );
    }
}
