//! R1 存储边界实验，不作为业务接口或生产数据库迁移。
//!
//! 仅在已授权的 Staging 环境启用 dependency-probes 后执行。
//! 必需配置为 R1_ALLOW_LIVE_PROBES=1、DATABASE_URL、
//! S3_STORAGE_ENDPOINT/REGION/BUCKET、S3_STORAGE_ACCESS_KEY_ID
//! 和 S3_STORAGE_SECRET_ACCESS_KEY。测试桶要求 Referer 时沿用 S3_STORAGE_REFERER。
//! 可选 R1_S3_ADDRESSING_STYLE 为 path（默认）或 virtual，不读取 dotenv 文件。
//! 标准输出采用 JSONL，包含不含秘密信息的精确清理清单。
//! 收到 SIGINT/SIGTERM 后等待当前检查结束再清理。SIGKILL、进程异常终止、
//! 主机失联或远端写入未确认时，无法保证自动清理完成。

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::future::Future;
use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use object_store::aws::AmazonS3Builder;
use object_store::path::Path as ObjectPath;
use object_store::{ClientOptions, ObjectStore, RetryConfig};
use reqwest::header::{HeaderMap, HeaderValue, REFERER};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions, PgPool, Postgres, Transaction};
use tokio::sync::Barrier;
use tokio::time::timeout;
use uuid::Uuid;

const IO_TIMEOUT: Duration = Duration::from_secs(15);
const CHECK_TIMEOUT: Duration = Duration::from_secs(60);
const CHECK_NAMES: &[&str] = &[
    "pg_atomic_admission",
    "pg_fault_rollback_and_retry",
    "pg_concurrent_idempotency",
    "pg_fingerprint_conflict",
    "pg_tenant_foreign_keys",
    "pg_stale_generation",
    "pg_cancelled_run",
    "pg_expired_lease",
    "pg_wrong_lease_owner",
    "pg_changed_plan_revision",
    "pg_changed_input_revision",
    "s3_put_head_get_ready",
    "s3_missing_object_rejected",
    "s3_digest_mismatch_rejected",
    "s3_put_db_rollback_unpublished",
];

type ProbeResult<T> = Result<T, ProbeError>;

// Errors contain only fixed codes; never retain a driver error, URL or credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProbeError {
    code: &'static str,
    stage: &'static str,
}

impl ProbeError {
    const fn new(code: &'static str, stage: &'static str) -> Self {
        Self { code, stage }
    }
}

impl From<sqlx::Error> for ProbeError {
    fn from(error: sqlx::Error) -> Self {
        let code = match &error {
            sqlx::Error::Database(error) => match error.code().as_deref() {
                Some("23503") => "PG_FOREIGN_KEY_VIOLATION",
                Some("23505") => "PG_UNIQUE_VIOLATION",
                Some("23514") => "PG_CHECK_VIOLATION",
                Some("42501") => "PG_PERMISSION_DENIED",
                Some("57014") => "PG_STATEMENT_CANCELLED",
                Some("55P03") => "PG_LOCK_TIMEOUT",
                Some("40001") => "PG_SERIALIZATION_FAILURE",
                Some("40P01") => "PG_DEADLOCK",
                _ => "PG_DATABASE_ERROR",
            },
            sqlx::Error::PoolTimedOut => "PG_POOL_TIMEOUT",
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) => "PG_CONNECTION_ERROR",
            _ => "PG_DRIVER_ERROR",
        };
        Self::new(code, "postgres")
    }
}

fn storage_error(error: object_store::Error, stage: &'static str) -> ProbeError {
    let code = match error {
        object_store::Error::NotFound { .. } => "S3_NOT_FOUND",
        object_store::Error::PermissionDenied { .. } => "S3_PERMISSION_DENIED",
        object_store::Error::Unauthenticated { .. } => "S3_UNAUTHENTICATED",
        object_store::Error::NotSupported { .. } => "S3_NOT_SUPPORTED",
        _ => "S3_REQUEST_FAILED",
    };
    ProbeError::new(code, stage)
}

fn ensure(condition: bool, code: &'static str) -> ProbeResult<()> {
    if condition {
        Ok(())
    } else {
        Err(ProbeError::new(code, "assertion"))
    }
}

fn expect_sqlstate<T>(result: Result<T, sqlx::Error>, expected: &str) -> ProbeResult<()> {
    match result {
        Err(sqlx::Error::Database(error)) if error.code().as_deref() == Some(expected) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => Err(ProbeError::new(
            "EXPECTED_SQL_REJECTION_MISSING",
            "assertion",
        )),
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn live_allowed(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

fn required_env(name: &'static str) -> ProbeResult<String> {
    env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or(ProbeError::new("MISSING_OR_INVALID_ENV", name))
}

fn validate_endpoint(endpoint: &str) -> ProbeResult<()> {
    let parsed = url::Url::parse(endpoint)
        .map_err(|_| ProbeError::new("INVALID_S3_ENDPOINT", "configuration"))?;
    ensure(
        parsed.scheme() == "https"
            && parsed.host_str().is_some()
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.port() != Some(0),
        "INVALID_S3_ENDPOINT",
    )
}

#[derive(Clone)]
struct Scope {
    probe_id: String,
    schema_name: String,
    owner_marker: String,
    object_prefix: String,
}

impl Scope {
    fn new() -> Self {
        let id = Uuid::new_v4();
        Self {
            probe_id: id.to_string(),
            schema_name: format!("r1_{}", id.simple()),
            owner_marker: format!("r1-storage-probe:{}", Uuid::new_v4()),
            object_prefix: format!("r1-20260918-{id}"),
        }
    }

    fn schema(&self) -> String {
        format!("\"{}\"", self.schema_name)
    }

    fn object_key(&self, tenant_id: &str, run_id: &str, digest: &str) -> ProbeResult<ObjectPath> {
        ensure(
            Uuid::parse_str(tenant_id).is_ok()
                && Uuid::parse_str(run_id).is_ok()
                && digest.len() == 64
                && digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "INVALID_INTERNAL_OBJECT_IDENTITY",
        )?;
        ObjectPath::parse(format!(
            "{}/{}/{}/attempt-1/{}/{}",
            self.object_prefix,
            tenant_id,
            run_id,
            Uuid::new_v4(),
            digest
        ))
        .map_err(|_| ProbeError::new("INVALID_INTERNAL_OBJECT_KEY", "isolation"))
    }

    fn owns_key(&self, key: &ObjectPath) -> bool {
        key.as_ref()
            .starts_with(&format!("{}/", self.object_prefix))
    }
}

struct Reporter {
    probe_id: String,
    started: Instant,
    failures: AtomicUsize,
    skipped: AtomicUsize,
    output_failed: AtomicBool,
    reported_checks: Mutex<BTreeSet<String>>,
}

impl Reporter {
    fn new(probe_id: String) -> Self {
        Self {
            probe_id,
            started: Instant::now(),
            failures: AtomicUsize::new(0),
            skipped: AtomicUsize::new(0),
            output_failed: AtomicBool::new(false),
            reported_checks: Mutex::new(BTreeSet::new()),
        }
    }

    fn emit(&self, check: &str, status: &str, details: Value) {
        if matches!(status, "pass" | "fail" | "skipped") {
            self.reported_checks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(check.to_owned());
        }
        if status == "fail" {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }
        if status == "skipped" {
            self.skipped.fetch_add(1, Ordering::Relaxed);
        }
        let line = json!({
            "probe": "r1_storage", "probe_id": self.probe_id,
            "check": check, "status": status,
            "elapsed_ms": self.started.elapsed().as_millis(), "details": details,
        });
        let mut output = io::stdout().lock();
        if writeln!(output, "{line}")
            .and_then(|_| output.flush())
            .is_err()
        {
            self.output_failed.store(true, Ordering::Relaxed);
            // A broken output pipe must not panic and bypass resource cleanup.
            let _ = writeln!(
                io::stderr().lock(),
                "{{\"probe\":\"r1_storage\",\"status\":\"fail\",\"code\":\"RESULT_OUTPUT_FAILED\"}}"
            );
        }
    }

    fn error(&self, check: &str, error: ProbeError) {
        self.emit(
            check,
            "fail",
            json!({"code": error.code, "stage": error.stage}),
        );
    }

    fn succeeded(&self) -> bool {
        self.failures.load(Ordering::Relaxed) == 0
            && self.skipped.load(Ordering::Relaxed) == 0
            && !self.output_failed.load(Ordering::Relaxed)
    }

    fn report_unfinished_checks(&self) {
        let reported = self
            .reported_checks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        for name in CHECK_NAMES {
            if !reported.contains(*name) {
                self.emit(
                    name,
                    "skipped",
                    json!({"code": "WORKER_EXITED_BEFORE_RESULT"}),
                );
            }
        }
    }
}

#[derive(Clone)]
struct ObjectIntent {
    key: ObjectPath,
    put_outcome: PutOutcome,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum PutOutcome {
    Unknown,
    Acknowledged,
    Rejected,
}

fn storage_client_options(referer: Option<&str>) -> ProbeResult<ClientOptions> {
    let mut headers = HeaderMap::new();
    if let Some(value) = referer {
        validate_endpoint(value)?;
        let mut header = HeaderValue::from_str(value)
            .map_err(|_| ProbeError::new("INVALID_S3_REFERER", "configuration"))?;
        header.set_sensitive(true);
        headers.insert(REFERER, header);
    }
    Ok(ClientOptions::new()
        .with_allow_http(false)
        .with_connect_timeout(Duration::from_secs(5))
        .with_timeout(IO_TIMEOUT)
        .with_default_headers(headers))
}

#[derive(Default)]
struct CleanupInventory {
    schema_attempted: bool,
    schema_commit_started: bool,
    schema_commit_acknowledged: bool,
    objects: Vec<ObjectIntent>,
}

struct Probe {
    scope: Scope,
    pool: PgPool,
    store: Arc<dyn ObjectStore>,
    inventory: Mutex<CleanupInventory>,
    reporter: Arc<Reporter>,
    stopping: AtomicBool,
}

impl Probe {
    fn from_env(scope: Scope, reporter: Arc<Reporter>) -> ProbeResult<Self> {
        let database_url = required_env("DATABASE_URL")?;
        let endpoint = required_env("S3_STORAGE_ENDPOINT")?;
        validate_endpoint(&endpoint)?;
        let region = required_env("S3_STORAGE_REGION")?;
        let bucket = required_env("S3_STORAGE_BUCKET")?;
        let access_key = required_env("S3_STORAGE_ACCESS_KEY_ID")?;
        let secret_key = required_env("S3_STORAGE_SECRET_ACCESS_KEY")?;
        let referer = env::var("S3_STORAGE_REFERER").ok();
        let virtual_hosted = match env::var("R1_S3_ADDRESSING_STYLE") {
            Err(env::VarError::NotPresent) => false,
            Ok(value) if value == "path" => false,
            Ok(value) if value == "virtual" => true,
            _ => {
                return Err(ProbeError::new(
                    "INVALID_S3_ADDRESSING_STYLE",
                    "configuration",
                ));
            }
        };
        let options: PgConnectOptions = database_url
            .parse()
            .map_err(|_| ProbeError::new("INVALID_DATABASE_URL", "configuration"))?;
        let options = options
            .disable_statement_logging()
            .application_name("r1_storage_probe")
            .options([
                ("search_path", "pg_catalog"),
                ("statement_timeout", "8000"),
                ("lock_timeout", "5000"),
                ("idle_in_transaction_session_timeout", "15000"),
            ]);
        let pool = PgPoolOptions::new()
            .max_connections(6)
            .min_connections(0)
            .acquire_timeout(Duration::from_secs(10))
            .connect_lazy_with(options);
        let store = AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_region(region)
            .with_bucket_name(bucket)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret_key)
            .with_virtual_hosted_style_request(virtual_hosted)
            .with_client_options(storage_client_options(referer.as_deref())?)
            .with_retry(RetryConfig {
                max_retries: 0,
                retry_timeout: IO_TIMEOUT,
                ..Default::default()
            })
            .build()
            .map_err(|error| storage_error(error, "s3_configuration"))?;
        Ok(Self {
            scope,
            pool,
            store: Arc::new(store),
            inventory: Mutex::new(CleanupInventory::default()),
            reporter,
            stopping: AtomicBool::new(false),
        })
    }

    fn inventory(&self) -> MutexGuard<'_, CleanupInventory> {
        self.inventory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn should_stop(&self) -> bool {
        self.stopping.load(Ordering::Relaxed) || self.reporter.output_failed.load(Ordering::Relaxed)
    }

    async fn check(&self, name: &str, future: impl Future<Output = ProbeResult<Value>>) -> bool {
        if self.should_stop() {
            self.reporter
                .emit(name, "skipped", json!({"code": "PROBE_STOPPING"}));
            return false;
        }
        let started = Instant::now();
        match timeout(CHECK_TIMEOUT, future).await {
            Ok(Ok(details)) => {
                self.reporter.emit(
                    name,
                    "pass",
                    json!({"duration_ms": started.elapsed().as_millis(), "observed": details}),
                );
                true
            }
            Ok(Err(error)) => {
                self.reporter.error(name, error);
                false
            }
            Err(_) => {
                self.reporter
                    .error(name, ProbeError::new("CHECK_TIMEOUT", "execution"));
                false
            }
        }
    }

    async fn execute_checks(&self) {
        if !self
            .check("pg_schema_isolation", self.create_schema())
            .await
        {
            for name in CHECK_NAMES {
                self.reporter
                    .emit(name, "skipped", json!({"code": "SCHEMA_SETUP_FAILED"}));
            }
            return;
        }
        self.check(CHECK_NAMES[0], self.check_atomic_admission())
            .await;
        self.check(CHECK_NAMES[1], self.check_admission_rollback())
            .await;
        self.check(CHECK_NAMES[2], self.check_concurrent_admission())
            .await;
        self.check(CHECK_NAMES[3], self.check_fingerprint_conflict())
            .await;
        self.check(CHECK_NAMES[4], self.check_tenant_foreign_keys())
            .await;
        for (name, rejection) in CHECK_NAMES[5..11].iter().zip([
            Rejection::OldGeneration,
            Rejection::Cancelled,
            Rejection::ExpiredLease,
            Rejection::WrongOwner,
            Rejection::ChangedPlanRevision,
            Rejection::ChangedInputRevision,
        ]) {
            self.check(name, self.check_fenced_score(rejection)).await;
        }
        self.check(CHECK_NAMES[12], self.check_missing_object())
            .await;
        if !self
            .check("s3_cleanup_access", self.check_cleanup_access())
            .await
        {
            for index in [11, 13, 14] {
                self.reporter.emit(
                    CHECK_NAMES[index],
                    "skipped",
                    json!({"code": "S3_CLEANUP_ACCESS_UNVERIFIED"}),
                );
            }
            return;
        }
        self.check(CHECK_NAMES[11], self.check_ready_object()).await;
        self.check(CHECK_NAMES[13], self.check_digest_mismatch())
            .await;
        self.check(CHECK_NAMES[14], self.check_object_database_rollback())
            .await;
    }

    async fn check_cleanup_access(&self) -> ProbeResult<Value> {
        // Some providers accept DELETE for absent keys without checking write
        // permissions. Verify deletion of a small, recorded object we own.
        let key = ObjectPath::from(format!("{}/cleanup-canary", self.scope.object_prefix));
        let payload = b"r1 cleanup probe";
        self.upload(&key, payload).await?;
        self.verify_object(&key, payload).await?;
        self.delete_recorded_object(&ObjectIntent {
            key,
            put_outcome: PutOutcome::Acknowledged,
        })
        .await?;
        Ok(json!({"written_object_deleted": true, "size_bytes": payload.len()}))
    }

    async fn transaction(&self) -> ProbeResult<Transaction<'_, Postgres>> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
            .execute(&mut *tx)
            .await?;
        Ok(tx)
    }

    async fn create_schema(&self) -> ProbeResult<Value> {
        let s = self.scope.schema();
        self.inventory().schema_attempted = true;
        self.reporter.emit(
            "cleanup_inventory_schema",
            "info",
            json!({
                "schema": self.scope.schema_name, "ownership_marker": self.scope.owner_marker,
                "object_prefix": self.scope.object_prefix,
            }),
        );
        ensure(
            !self.reporter.output_failed.load(Ordering::Relaxed),
            "RESULT_OUTPUT_FAILED",
        )?;
        let mut tx = self.transaction().await?;
        // The ownership comment and schema commit together. A name collision is
        // never adopted; cleanup refuses any schema with a different marker.
        sqlx::raw_sql(&format!(
            "CREATE SCHEMA {s}; COMMENT ON SCHEMA {s} IS '{}';",
            self.scope.owner_marker
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::raw_sql(&format!(
            r#"
            CREATE TABLE {s}.plan_revisions (
                tenant_id text NOT NULL, plan_id text NOT NULL,
                plan_revision bigint NOT NULL CHECK (plan_revision > 0),
                PRIMARY KEY (tenant_id, plan_id, plan_revision)
            );
            CREATE TABLE {s}.runs (
                tenant_id text NOT NULL, run_id text NOT NULL,
                plan_id text NOT NULL, plan_revision bigint NOT NULL,
                input_revision bigint NOT NULL DEFAULT 1,
                cancel_requested boolean NOT NULL DEFAULT false,
                current_score_revision bigint,
                PRIMARY KEY (tenant_id, run_id),
                FOREIGN KEY (tenant_id, plan_id, plan_revision)
                    REFERENCES {s}.plan_revisions (tenant_id, plan_id, plan_revision)
            );
            CREATE TABLE {s}.jobs (
                tenant_id text NOT NULL, job_id text NOT NULL, run_id text NOT NULL,
                state text NOT NULL DEFAULT 'queued' CHECK (state IN ('queued', 'running')),
                execution_generation bigint NOT NULL DEFAULT 1,
                submitted_generation bigint,
                lease_owner text,
                lease_expires_at timestamptz,
                PRIMARY KEY (tenant_id, job_id),
                UNIQUE (tenant_id, run_id), UNIQUE (tenant_id, run_id, job_id),
                FOREIGN KEY (tenant_id, run_id) REFERENCES {s}.runs (tenant_id, run_id)
            );
            CREATE TABLE {s}.idempotency_receipts (
                tenant_id text NOT NULL, operation text NOT NULL,
                idempotency_key text NOT NULL, request_fingerprint text NOT NULL,
                run_id text NOT NULL, job_id text NOT NULL, response jsonb NOT NULL,
                PRIMARY KEY (tenant_id, operation, idempotency_key),
                FOREIGN KEY (tenant_id, run_id) REFERENCES {s}.runs (tenant_id, run_id)
                    DEFERRABLE INITIALLY DEFERRED,
                FOREIGN KEY (tenant_id, run_id, job_id)
                    REFERENCES {s}.jobs (tenant_id, run_id, job_id)
                    DEFERRABLE INITIALLY DEFERRED
            );
            CREATE TABLE {s}.score_revisions (
                tenant_id text NOT NULL, run_id text NOT NULL,
                score_revision bigint NOT NULL, execution_generation bigint NOT NULL,
                score_hundredths integer NOT NULL CHECK (score_hundredths BETWEEN 0 AND 10000),
                PRIMARY KEY (tenant_id, run_id, score_revision),
                UNIQUE (tenant_id, run_id, execution_generation),
                FOREIGN KEY (tenant_id, run_id) REFERENCES {s}.runs (tenant_id, run_id)
            );
            ALTER TABLE {s}.runs ADD CONSTRAINT current_score_exists
                FOREIGN KEY (tenant_id, run_id, current_score_revision)
                REFERENCES {s}.score_revisions (tenant_id, run_id, score_revision)
                DEFERRABLE INITIALLY DEFERRED;
            CREATE TABLE {s}.artifacts (
                tenant_id text NOT NULL, run_id text NOT NULL, artifact_id text NOT NULL,
                execution_generation bigint NOT NULL,
                object_key text NOT NULL UNIQUE, content_sha256 text NOT NULL,
                size_bytes bigint NOT NULL CHECK (size_bytes >= 0),
                state text NOT NULL CHECK (state IN ('pending', 'ready')),
                verified_at timestamptz,
                PRIMARY KEY (tenant_id, artifact_id),
                UNIQUE (tenant_id, run_id, artifact_id, state),
                CHECK ((state = 'ready') = (verified_at IS NOT NULL)),
                FOREIGN KEY (tenant_id, run_id) REFERENCES {s}.runs (tenant_id, run_id)
            );
            CREATE TABLE {s}.evidence_references (
                tenant_id text NOT NULL, run_id text NOT NULL, artifact_id text NOT NULL,
                artifact_state text NOT NULL DEFAULT 'ready' CHECK (artifact_state = 'ready'),
                PRIMARY KEY (tenant_id, run_id, artifact_id),
                FOREIGN KEY (tenant_id, run_id) REFERENCES {s}.runs (tenant_id, run_id),
                FOREIGN KEY (tenant_id, run_id, artifact_id, artifact_state)
                    REFERENCES {s}.artifacts (tenant_id, run_id, artifact_id, state)
            );
            CREATE TABLE {s}.publications (
                tenant_id text NOT NULL, run_id text NOT NULL, artifact_id text NOT NULL,
                artifact_state text NOT NULL DEFAULT 'ready' CHECK (artifact_state = 'ready'),
                publication_status text NOT NULL CHECK (publication_status = 'published'),
                PRIMARY KEY (tenant_id, run_id),
                FOREIGN KEY (tenant_id, run_id) REFERENCES {s}.runs (tenant_id, run_id),
                FOREIGN KEY (tenant_id, run_id, artifact_id, artifact_state)
                    REFERENCES {s}.artifacts (tenant_id, run_id, artifact_id, state)
            );
        "#
        ))
        .execute(&mut *tx)
        .await?;
        self.inventory().schema_commit_started = true;
        tx.commit().await?;
        self.inventory().schema_commit_acknowledged = true;
        let owned = self.schema_owner().await?;
        ensure(
            owned.as_deref() == Some(self.scope.owner_marker.as_str()),
            "SCHEMA_OWNER_MISMATCH",
        )?;
        Ok(json!({"schema": self.scope.schema_name, "ownership_verified": true}))
    }

    async fn schema_owner(&self) -> ProbeResult<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as(
            "SELECT pg_catalog.obj_description(oid, 'pg_namespace') FROM pg_catalog.pg_namespace WHERE nspname = $1"
        ).bind(&self.scope.schema_name).fetch_optional(&self.pool).await?;
        Ok(row.map(|(marker,)| marker.unwrap_or_default()))
    }

    async fn new_admission(&self) -> ProbeResult<Admission> {
        let admission = Admission {
            tenant_id: Uuid::new_v4().to_string(),
            plan_id: Uuid::new_v4().to_string(),
            key: Uuid::new_v4().to_string(),
            fingerprint: sha256(b"r1 fixed synthetic admission, revision 1"),
        };
        sqlx::query(&format!(
            "INSERT INTO {}.plan_revisions VALUES ($1, $2, 1)",
            self.scope.schema()
        ))
        .bind(&admission.tenant_id)
        .bind(&admission.plan_id)
        .execute(&self.pool)
        .await?;
        Ok(admission)
    }

    async fn admit(
        &self,
        admission: &Admission,
        mode: AdmissionMode,
        barrier: Option<&Barrier>,
    ) -> ProbeResult<Receipt> {
        let s = self.scope.schema();
        let run_id = Uuid::new_v4().to_string();
        let job_id = Uuid::new_v4().to_string();
        let response = json!({"run_id": run_id, "job_id": job_id, "state": "queued"});
        let mut tx = self.transaction().await?;
        let session_id: i32 = sqlx::query_scalar("SELECT pg_catalog.pg_backend_pid()")
            .fetch_one(&mut *tx)
            .await?;
        if let Some(barrier) = barrier {
            timeout(Duration::from_secs(10), barrier.wait())
                .await
                .map_err(|_| ProbeError::new("CONCURRENT_START_TIMEOUT", "admission"))?;
        }
        // Reserve the receipt first. Its deferred FKs make it impossible to
        // commit a receipt without its Run and Job; the unique key serialises retries.
        let inserted = sqlx::query(&format!(r#"
            INSERT INTO {s}.idempotency_receipts
                (tenant_id, operation, idempotency_key, request_fingerprint, run_id, job_id, response)
            VALUES ($1, 'create_review', $2, $3, $4, $5, $6)
            ON CONFLICT (tenant_id, operation, idempotency_key) DO NOTHING
        "#)).bind(&admission.tenant_id).bind(&admission.key).bind(&admission.fingerprint)
            .bind(&run_id).bind(&job_id).bind(&response).execute(&mut *tx).await?.rows_affected();
        if inserted == 0 {
            let (fingerprint, run_id, job_id, response): (String, String, String, Value) = sqlx::query_as(&format!(
                "SELECT request_fingerprint, run_id, job_id, response FROM {s}.idempotency_receipts WHERE tenant_id=$1 AND operation='create_review' AND idempotency_key=$2"
            )).bind(&admission.tenant_id).bind(&admission.key).fetch_one(&mut *tx).await?;
            if fingerprint != admission.fingerprint {
                tx.rollback().await?;
                return Err(ProbeError::new("IDEMPOTENCY_CONFLICT", "admission"));
            }
            tx.commit().await?;
            return Ok(Receipt {
                run_id,
                job_id,
                response,
                created: false,
                session_id,
            });
        }
        sqlx::query(&format!(
            "INSERT INTO {s}.runs (tenant_id, run_id, plan_id, plan_revision) VALUES ($1, $2, $3, 1)"
        )).bind(&admission.tenant_id).bind(&run_id).bind(&admission.plan_id).execute(&mut *tx).await?;
        sqlx::query(&format!(
            "INSERT INTO {s}.jobs (tenant_id, job_id, run_id) VALUES ($1, $2, $3)"
        ))
        .bind(&admission.tenant_id)
        .bind(&job_id)
        .bind(&run_id)
        .execute(&mut *tx)
        .await?;
        if matches!(mode, AdmissionMode::ObserveUncommitted) {
            ensure(
                self.admission_counts(admission).await? == (0, 0, 0),
                "UNCOMMITTED_ADMISSION_VISIBLE",
            )?;
        }
        if matches!(mode, AdmissionMode::FaultAfterWrites) {
            let failure = sqlx::query("SELECT 1 / 0").execute(&mut *tx).await;
            tx.rollback().await?;
            expect_sqlstate(failure, "22012")?;
            return Err(ProbeError::new("INJECTED_TRANSACTION_FAILURE", "admission"));
        }
        tx.commit().await?;
        Ok(Receipt {
            run_id,
            job_id,
            response,
            created: true,
            session_id,
        })
    }

    async fn admission_counts(&self, admission: &Admission) -> ProbeResult<(i64, i64, i64)> {
        let s = self.scope.schema();
        Ok(sqlx::query_as(&format!(
            r#"SELECT
            (SELECT count(*) FROM {s}.runs WHERE tenant_id=$1),
            (SELECT count(*) FROM {s}.jobs WHERE tenant_id=$1),
            (SELECT count(*) FROM {s}.idempotency_receipts WHERE tenant_id=$1)
        "#
        ))
        .bind(&admission.tenant_id)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn fixture(&self) -> ProbeResult<(Admission, Receipt)> {
        let admission = self.new_admission().await?;
        let receipt = self.admit(&admission, AdmissionMode::Normal, None).await?;
        Ok((admission, receipt))
    }

    async fn check_atomic_admission(&self) -> ProbeResult<Value> {
        let admission = self.new_admission().await?;
        let first = self
            .admit(&admission, AdmissionMode::ObserveUncommitted, None)
            .await?;
        // The caller deliberately ignores the first reply, then replays the request.
        let replay = self.admit(&admission, AdmissionMode::Normal, None).await?;
        let counts = self.admission_counts(&admission).await?;
        ensure(
            counts == (1, 1, 1)
                && first.created
                && !replay.created
                && first.response == replay.response,
            "ADMISSION_NOT_ATOMIC_OR_REPLAY_CHANGED",
        )?;
        Ok(json!({"counts": counts, "invisible_before_commit": true, "lost_reply_replayed": true}))
    }

    async fn check_admission_rollback(&self) -> ProbeResult<Value> {
        let admission = self.new_admission().await?;
        let failed = self
            .admit(&admission, AdmissionMode::FaultAfterWrites, None)
            .await;
        ensure(
            matches!(
                failed,
                Err(ProbeError {
                    code: "INJECTED_TRANSACTION_FAILURE",
                    ..
                })
            ),
            "INJECTED_FAULT_NOT_OBSERVED",
        )?;
        let after_rollback = self.admission_counts(&admission).await?;
        ensure(after_rollback == (0, 0, 0), "ADMISSION_ROLLBACK_LEFT_ROWS")?;
        let retry = self.admit(&admission, AdmissionMode::Normal, None).await?;
        let after_retry = self.admission_counts(&admission).await?;
        ensure(
            retry.created && after_retry == (1, 1, 1),
            "ADMISSION_RETRY_NOT_RECOVERED",
        )?;
        Ok(
            json!({"after_rollback": after_rollback, "after_retry": after_retry, "injected_sqlstate": "22012"}),
        )
    }

    async fn check_concurrent_admission(&self) -> ProbeResult<Value> {
        let admission = self.new_admission().await?;
        let barrier = Barrier::new(4);
        // join! drains all four futures, including failures; no detached writer
        // can race schema cleanup. Each reaches the barrier inside its own tx.
        let (a, b, c, d) = tokio::join!(
            self.admit(&admission, AdmissionMode::Normal, Some(&barrier)),
            self.admit(&admission, AdmissionMode::Normal, Some(&barrier)),
            self.admit(&admission, AdmissionMode::Normal, Some(&barrier)),
            self.admit(&admission, AdmissionMode::Normal, Some(&barrier)),
        );
        let receipts = [a?, b?, c?, d?];
        let mut sessions: Vec<i32> = receipts.iter().map(|receipt| receipt.session_id).collect();
        sessions.sort_unstable();
        sessions.dedup();
        let created = receipts.iter().filter(|receipt| receipt.created).count();
        let counts = self.admission_counts(&admission).await?;
        ensure(
            sessions.len() == 4
                && created == 1
                && counts == (1, 1, 1)
                && receipts
                    .iter()
                    .all(|receipt| receipt.response == receipts[0].response),
            "CONCURRENT_IDEMPOTENCY_BROKEN",
        )?;
        Ok(
            json!({"concurrent_sessions": sessions.len(), "created": created, "counts": counts, "identical_receipts": true}),
        )
    }

    async fn check_fingerprint_conflict(&self) -> ProbeResult<Value> {
        let (admission, first) = self.fixture().await?;
        let mut changed = admission.clone();
        changed.fingerprint = sha256(b"a different synthetic admission");
        let rejection = self.admit(&changed, AdmissionMode::Normal, None).await;
        ensure(
            matches!(
                rejection,
                Err(ProbeError {
                    code: "IDEMPOTENCY_CONFLICT",
                    ..
                })
            ),
            "FINGERPRINT_CONFLICT_NOT_REJECTED",
        )?;
        let replay = self.admit(&admission, AdmissionMode::Normal, None).await?;
        let counts = self.admission_counts(&admission).await?;
        ensure(
            counts == (1, 1, 1) && replay.response == first.response,
            "CONFLICT_CHANGED_ORIGINAL_RECEIPT",
        )?;
        // A different tenant may use the same key without adopting the first Run.
        let mut other_tenant = self.new_admission().await?;
        other_tenant.key.clone_from(&admission.key);
        let other = self
            .admit(&other_tenant, AdmissionMode::Normal, None)
            .await?;
        ensure(
            other.created && other.run_id != first.run_id,
            "IDEMPOTENCY_CROSSED_TENANTS",
        )?;
        Ok(
            json!({"conflict": "IDEMPOTENCY_CONFLICT", "original_counts": counts, "tenant_key_isolated": true}),
        )
    }

    async fn check_tenant_foreign_keys(&self) -> ProbeResult<Value> {
        let (a, receipt) = self.fixture().await?;
        let b = self.new_admission().await?;
        let s = self.scope.schema();
        expect_sqlstate(sqlx::query(&format!(
            "INSERT INTO {s}.runs (tenant_id, run_id, plan_id, plan_revision) VALUES ($1, $2, $3, 1)"
        )).bind(&b.tenant_id).bind(Uuid::new_v4().to_string()).bind(&a.plan_id).execute(&self.pool).await, "23503")?;
        expect_sqlstate(
            sqlx::query(&format!(
                "INSERT INTO {s}.jobs (tenant_id, job_id, run_id) VALUES ($1, $2, $3)"
            ))
            .bind(&b.tenant_id)
            .bind(Uuid::new_v4().to_string())
            .bind(&receipt.run_id)
            .execute(&self.pool)
            .await,
            "23503",
        )?;
        expect_sqlstate(
            sqlx::query(&format!(
                "INSERT INTO {s}.score_revisions VALUES ($1, $2, 1, 1, 8250)"
            ))
            .bind(&b.tenant_id)
            .bind(&receipt.run_id)
            .execute(&self.pool)
            .await,
            "23503",
        )?;
        ensure(
            self.admission_counts(&b).await? == (0, 0, 0),
            "CROSS_TENANT_ROWS_CREATED",
        )?;
        Ok(json!({"plan_run_fk": "23503", "run_job_fk": "23503", "run_score_fk": "23503"}))
    }

    async fn submit_score(
        &self,
        admission: &Admission,
        receipt: &Receipt,
        submission: ScoreSubmission,
    ) -> ProbeResult<bool> {
        let s = self.scope.schema();
        let mut tx = self.transaction().await?;
        // Lock both authorities. Lease revocation and cancellation must serialize
        // with the predicate, not merely with the later score insert.
        let locked: Option<(String,)> = sqlx::query_as(&format!(
            r#"
            SELECT r.run_id FROM {s}.runs r JOIN {s}.jobs j
                ON j.tenant_id=r.tenant_id AND j.run_id=r.run_id
            WHERE r.tenant_id=$1 AND r.run_id=$2 AND j.job_id=$3 FOR UPDATE OF r, j
        "#
        ))
        .bind(&admission.tenant_id)
        .bind(&receipt.run_id)
        .bind(&receipt.job_id)
        .fetch_optional(&mut *tx)
        .await?;
        ensure(locked.is_some(), "SCORE_FIXTURE_MISSING")?;
        let revision: Option<i64> = sqlx::query_scalar(&format!(
            r#"
            UPDATE {s}.runs r SET current_score_revision=coalesce(r.current_score_revision, 0)+1
            FROM {s}.jobs j
            WHERE r.tenant_id=$1 AND r.run_id=$2 AND j.tenant_id=r.tenant_id
                AND j.run_id=r.run_id AND j.job_id=$3
                AND NOT r.cancel_requested AND r.plan_revision=$4 AND r.input_revision=$5
                AND j.execution_generation=$6 AND j.lease_owner=$7 AND j.state='running'
                AND j.lease_expires_at > clock_timestamp()
                AND j.submitted_generation IS DISTINCT FROM j.execution_generation
            RETURNING r.current_score_revision
        "#
        ))
        .bind(&admission.tenant_id)
        .bind(&receipt.run_id)
        .bind(&receipt.job_id)
        .bind(submission.plan_revision)
        .bind(submission.input_revision)
        .bind(submission.generation)
        .bind(submission.owner)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(revision) = revision else {
            tx.rollback().await?;
            return Ok(false);
        };
        sqlx::query(&format!(
            "INSERT INTO {s}.score_revisions VALUES ($1, $2, $3, $4, $5)"
        ))
        .bind(&admission.tenant_id)
        .bind(&receipt.run_id)
        .bind(revision)
        .bind(submission.generation)
        .bind(submission.score_hundredths)
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!(
            "UPDATE {s}.jobs SET submitted_generation=execution_generation WHERE tenant_id=$1 AND job_id=$2"
        )).bind(&admission.tenant_id).bind(&receipt.job_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(true)
    }

    async fn check_fenced_score(&self, rejection: Rejection) -> ProbeResult<Value> {
        let (admission, receipt) = self.fixture().await?;
        let s = self.scope.schema();
        sqlx::query(&format!(
            r#"
            UPDATE {s}.jobs SET state='running', lease_owner='r1-worker',
                lease_expires_at=clock_timestamp()+interval '5 minutes'
            WHERE tenant_id=$1 AND job_id=$2
        "#
        ))
        .bind(&admission.tenant_id)
        .bind(&receipt.job_id)
        .execute(&self.pool)
        .await?;
        let first = ScoreSubmission::baseline();
        ensure(
            self.submit_score(&admission, &receipt, first).await?,
            "CURRENT_ATTEMPT_REJECTED",
        )?;
        ensure(
            !self.submit_score(&admission, &receipt, first).await?,
            "DUPLICATE_SCORE_ACCEPTED",
        )?;
        sqlx::query(&format!(
            "UPDATE {s}.jobs SET execution_generation=2 WHERE tenant_id=$1 AND job_id=$2"
        ))
        .bind(&admission.tenant_id)
        .bind(&receipt.job_id)
        .execute(&self.pool)
        .await?;
        let mut submission = ScoreSubmission {
            generation: 2,
            score_hundredths: 9900,
            ..first
        };
        match rejection {
            Rejection::OldGeneration => submission.generation = 1,
            Rejection::Cancelled => {
                sqlx::query(&format!(
                    "UPDATE {s}.runs SET cancel_requested=true WHERE tenant_id=$1 AND run_id=$2"
                ))
                .bind(&admission.tenant_id)
                .bind(&receipt.run_id)
                .execute(&self.pool)
                .await?;
            }
            Rejection::ExpiredLease => {
                sqlx::query(&format!("UPDATE {s}.jobs SET lease_expires_at=clock_timestamp()-interval '1 second' WHERE tenant_id=$1 AND job_id=$2"))
                    .bind(&admission.tenant_id).bind(&receipt.job_id).execute(&self.pool).await?;
            }
            Rejection::WrongOwner => submission.owner = "r1-previous-worker",
            Rejection::ChangedPlanRevision => submission.plan_revision = 2,
            Rejection::ChangedInputRevision => submission.input_revision = 2,
        }
        ensure(
            !self.submit_score(&admission, &receipt, submission).await?,
            "FENCED_SUBMISSION_ACCEPTED",
        )?;
        let (revision, score, count): (i64, i32, i64) = sqlx::query_as(&format!(r#"
            SELECT r.current_score_revision, v.score_hundredths,
                (SELECT count(*) FROM {s}.score_revisions WHERE tenant_id=$1 AND run_id=$2)
            FROM {s}.runs r JOIN {s}.score_revisions v
                ON v.tenant_id=r.tenant_id AND v.run_id=r.run_id AND v.score_revision=r.current_score_revision
            WHERE r.tenant_id=$1 AND r.run_id=$2
        "#)).bind(&admission.tenant_id).bind(&receipt.run_id).fetch_one(&self.pool).await?;
        ensure(
            (revision, score, count) == (1, 8250, 1),
            "EXISTING_SCORE_CHANGED",
        )?;
        let current_generation_control = if matches!(rejection, Rejection::OldGeneration) {
            let current = ScoreSubmission {
                generation: 2,
                score_hundredths: 9100,
                ..first
            };
            ensure(
                self.submit_score(&admission, &receipt, current).await?,
                "CURRENT_GENERATION_NOT_ACCEPTED",
            )?;
            ensure(
                !self.submit_score(&admission, &receipt, current).await?,
                "CURRENT_GENERATION_DUPLICATED",
            )?;
            let (current_revision, old_score, new_score): (i64, i32, i32) =
                sqlx::query_as(&format!(
                    r#"
                SELECT r.current_score_revision, old.score_hundredths, new.score_hundredths
                FROM {s}.runs r JOIN {s}.score_revisions old
                    ON old.tenant_id=r.tenant_id AND old.run_id=r.run_id AND old.score_revision=1
                JOIN {s}.score_revisions new
                    ON new.tenant_id=r.tenant_id AND new.run_id=r.run_id AND new.score_revision=2
                WHERE r.tenant_id=$1 AND r.run_id=$2
            "#
                ))
                .bind(&admission.tenant_id)
                .bind(&receipt.run_id)
                .fetch_one(&self.pool)
                .await?;
            ensure(
                (current_revision, old_score, new_score) == (2, 8250, 9100),
                "CURRENT_GENERATION_REPLACED_HISTORY",
            )?;
            Some(
                json!({"score_revision": current_revision, "previous_score_hundredths": old_score, "score_hundredths": new_score}),
            )
        } else {
            None
        };
        Ok(
            json!({"initial_commit": true, "duplicate_rejected": true, "late_commit": false,
            "after_rejection": {"score_revision": revision, "score_hundredths": score, "snapshot_count": count},
            "current_generation_control": current_generation_control}),
        )
    }

    async fn upload(&self, key: &ObjectPath, bytes: &[u8]) -> ProbeResult<()> {
        ensure(self.scope.owns_key(key), "OBJECT_OUTSIDE_PROBE_PREFIX")?;
        {
            let mut inventory = self.inventory();
            ensure(
                !inventory.objects.iter().any(|intent| intent.key == *key),
                "OBJECT_KEY_REUSE",
            )?;
            inventory.objects.push(ObjectIntent {
                key: key.clone(),
                put_outcome: PutOutcome::Unknown,
            });
        }
        self.reporter.emit(
            "cleanup_inventory_object",
            "info",
            json!({"object_key": key.as_ref(), "put_acknowledged": false}),
        );
        ensure(
            !self.reporter.output_failed.load(Ordering::Relaxed),
            "RESULT_OUTPUT_FAILED",
        )?;
        // Ordinary PUT is sufficient: each object gets a fresh random key and a
        // digest. The probe never overwrites/retries a key with different bytes.
        let result = timeout(IO_TIMEOUT, self.store.put(key, bytes.to_vec().into()))
            .await
            .map_err(|_| ProbeError::new("S3_PUT_OUTCOME_UNKNOWN", "s3_put"))?;
        if let Err(error) = result {
            // An explicit rejection is distinct from a timed-out write with an
            // unknown remote outcome. Keep the key inventory in both cases.
            if matches!(
                error,
                object_store::Error::PermissionDenied { .. }
                    | object_store::Error::Unauthenticated { .. }
                    | object_store::Error::NotSupported { .. }
            ) && let Some(intent) = self
                .inventory()
                .objects
                .iter_mut()
                .find(|intent| intent.key == *key)
            {
                intent.put_outcome = PutOutcome::Rejected;
            }
            return Err(storage_error(error, "s3_put"));
        }
        if let Some(intent) = self
            .inventory()
            .objects
            .iter_mut()
            .find(|intent| intent.key == *key)
        {
            intent.put_outcome = PutOutcome::Acknowledged;
        }
        self.reporter.emit(
            "s3_put_acknowledged",
            "info",
            json!({"object_key": key.as_ref()}),
        );
        Ok(())
    }

    async fn verify_object(
        &self,
        key: &ObjectPath,
        expected: &[u8],
    ) -> ProbeResult<VerifiedObject> {
        ensure(self.scope.owns_key(key), "OBJECT_OUTSIDE_PROBE_PREFIX")?;
        let head = timeout(IO_TIMEOUT, self.store.head(key))
            .await
            .map_err(|_| ProbeError::new("S3_TIMEOUT", "s3_head"))?
            .map_err(|error| storage_error(error, "s3_head"))?;
        ensure(head.size == expected.len() as u64, "S3_HEAD_SIZE_MISMATCH")?;
        let bytes = timeout(IO_TIMEOUT, async {
            let result = self
                .store
                .get(key)
                .await
                .map_err(|error| storage_error(error, "s3_get"))?;
            ensure(
                result.meta.size == expected.len() as u64,
                "S3_GET_SIZE_MISMATCH",
            )?;
            result
                .bytes()
                .await
                .map_err(|error| storage_error(error, "s3_get_body"))
        })
        .await
        .map_err(|_| ProbeError::new("S3_TIMEOUT", "s3_get"))??;
        ensure(bytes.len() == expected.len(), "S3_BODY_SIZE_MISMATCH")?;
        let digest = sha256(&bytes);
        ensure(digest == sha256(expected), "S3_DIGEST_MISMATCH")?;
        Ok(VerifiedObject {
            key: key.clone(),
            digest,
            size_bytes: bytes.len() as i64,
        })
    }

    async fn register_ready(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        admission: &Admission,
        receipt: &Receipt,
        verified: &VerifiedObject,
    ) -> ProbeResult<String> {
        let artifact_id = Uuid::new_v4().to_string();
        sqlx::query(&format!(r#"
            INSERT INTO {}.artifacts
                (tenant_id, run_id, artifact_id, execution_generation, object_key, content_sha256, size_bytes, state, verified_at)
            VALUES ($1, $2, $3, 1, $4, $5, $6, 'ready', clock_timestamp())
        "#, self.scope.schema())).bind(&admission.tenant_id).bind(&receipt.run_id).bind(&artifact_id)
            .bind(verified.key.as_ref()).bind(&verified.digest).bind(verified.size_bytes)
            .execute(&mut **tx).await?;
        Ok(artifact_id)
    }

    async fn reference_and_publish(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        admission: &Admission,
        receipt: &Receipt,
        artifact_id: &str,
    ) -> ProbeResult<()> {
        let s = self.scope.schema();
        sqlx::query(&format!("INSERT INTO {s}.evidence_references (tenant_id, run_id, artifact_id) VALUES ($1, $2, $3)"))
            .bind(&admission.tenant_id).bind(&receipt.run_id).bind(artifact_id).execute(&mut **tx).await?;
        sqlx::query(&format!("INSERT INTO {s}.publications (tenant_id, run_id, artifact_id, publication_status) VALUES ($1, $2, $3, 'published')"))
            .bind(&admission.tenant_id).bind(&receipt.run_id).bind(artifact_id).execute(&mut **tx).await?;
        Ok(())
    }

    async fn artifact_counts(&self, admission: &Admission) -> ProbeResult<(i64, i64, i64)> {
        let s = self.scope.schema();
        Ok(sqlx::query_as(&format!(
            r#"SELECT
            (SELECT count(*) FROM {s}.artifacts WHERE tenant_id=$1 AND state='ready'),
            (SELECT count(*) FROM {s}.evidence_references WHERE tenant_id=$1),
            (SELECT count(*) FROM {s}.publications WHERE tenant_id=$1)
        "#
        ))
        .bind(&admission.tenant_id)
        .fetch_one(&self.pool)
        .await?)
    }

    async fn stored_object(
        &self,
        admission: &Admission,
        receipt: &Receipt,
    ) -> ProbeResult<VerifiedObject> {
        let payload = format!("r1 synthetic evidence {}", Uuid::new_v4()).into_bytes();
        let key =
            self.scope
                .object_key(&admission.tenant_id, &receipt.run_id, &sha256(&payload))?;
        self.upload(&key, &payload).await?;
        self.verify_object(&key, &payload).await
    }

    async fn check_ready_object(&self) -> ProbeResult<Value> {
        let (admission, receipt) = self.fixture().await?;
        let verified = self.stored_object(&admission, &receipt).await?;
        ensure(
            self.artifact_counts(&admission).await? == (0, 0, 0),
            "OBJECT_PUBLISHED_BEFORE_REGISTRATION",
        )?;
        let mut tx = self.transaction().await?;
        let artifact_id = self
            .register_ready(&mut tx, &admission, &receipt, &verified)
            .await?;
        self.reference_and_publish(&mut tx, &admission, &receipt, &artifact_id)
            .await?;
        tx.commit().await?;
        let counts = self.artifact_counts(&admission).await?;
        let (digest, size): (String, i64) = sqlx::query_as(&format!(
            "SELECT content_sha256, size_bytes FROM {}.artifacts WHERE tenant_id=$1 AND artifact_id=$2", self.scope.schema()
        )).bind(&admission.tenant_id).bind(&artifact_id).fetch_one(&self.pool).await?;
        ensure(
            counts == (1, 1, 1) && digest == verified.digest && size == verified.size_bytes,
            "READY_REFERENCE_NOT_PERSISTED",
        )?;
        let (other, other_receipt) = self.fixture().await?;
        expect_sqlstate(sqlx::query(&format!(
            "INSERT INTO {}.evidence_references (tenant_id, run_id, artifact_id) VALUES ($1, $2, $3)", self.scope.schema()
        )).bind(&other.tenant_id).bind(&other_receipt.run_id).bind(&artifact_id).execute(&self.pool).await, "23503")?;
        Ok(
            json!({"counts": counts, "content_sha256": digest, "size_bytes": size, "cross_tenant_artifact_fk": "23503"}),
        )
    }

    async fn check_missing_object(&self) -> ProbeResult<Value> {
        let (admission, receipt) = self.fixture().await?;
        let payload = b"r1 object deliberately never uploaded";
        let key = self
            .scope
            .object_key(&admission.tenant_id, &receipt.run_id, &sha256(payload))?;
        let verification = self.verify_object(&key, payload).await;
        match verification {
            Err(ProbeError {
                code: "S3_NOT_FOUND",
                ..
            }) => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(ProbeError::new("MISSING_OBJECT_VERIFIED", "assertion")),
        }
        let get = timeout(IO_TIMEOUT, self.store.get(&key))
            .await
            .map_err(|_| ProbeError::new("S3_TIMEOUT", "s3_missing_get"))?;
        match get {
            Err(object_store::Error::NotFound { .. }) => {}
            Err(error) => return Err(storage_error(error, "s3_missing_get")),
            Ok(_) => return Err(ProbeError::new("MISSING_OBJECT_READABLE", "assertion")),
        }
        let artifact_id = Uuid::new_v4().to_string();
        let s = self.scope.schema();
        let reference_sql = format!(
            "INSERT INTO {s}.evidence_references (tenant_id, run_id, artifact_id) VALUES ($1, $2, $3)"
        );
        expect_sqlstate(
            sqlx::query(&reference_sql)
                .bind(&admission.tenant_id)
                .bind(&receipt.run_id)
                .bind(&artifact_id)
                .execute(&self.pool)
                .await,
            "23503",
        )?;
        sqlx::query(&format!(r#"
            INSERT INTO {s}.artifacts
                (tenant_id, run_id, artifact_id, execution_generation, object_key, content_sha256, size_bytes, state)
            VALUES ($1, $2, $3, 1, $4, $5, $6, 'pending')
        "#)).bind(&admission.tenant_id).bind(&receipt.run_id).bind(&artifact_id).bind(key.as_ref())
            .bind(sha256(payload)).bind(payload.len() as i64).execute(&self.pool).await?;
        expect_sqlstate(
            sqlx::query(&reference_sql)
                .bind(&admission.tenant_id)
                .bind(&receipt.run_id)
                .bind(&artifact_id)
                .execute(&self.pool)
                .await,
            "23503",
        )?;
        expect_sqlstate(sqlx::query(&format!(
            "INSERT INTO {s}.publications (tenant_id, run_id, artifact_id, publication_status) VALUES ($1, $2, $3, 'published')"
        )).bind(&admission.tenant_id).bind(&receipt.run_id).bind(&artifact_id).execute(&self.pool).await, "23503")?;
        let counts = self.artifact_counts(&admission).await?;
        ensure(counts == (0, 0, 0), "MISSING_OBJECT_REFERENCED")?;
        Ok(
            json!({"head": "S3_NOT_FOUND", "get": "S3_NOT_FOUND", "missing_and_pending_fk": "23503", "counts": counts}),
        )
    }

    async fn check_digest_mismatch(&self) -> ProbeResult<Value> {
        let (admission, receipt) = self.fixture().await?;
        let payload = b"r1 body A";
        let key = self
            .scope
            .object_key(&admission.tenant_id, &receipt.run_id, &sha256(payload))?;
        self.upload(&key, payload).await?;
        let verification = self.verify_object(&key, b"r1 body B").await;
        match verification {
            Err(ProbeError {
                code: "S3_DIGEST_MISMATCH",
                ..
            }) => {}
            Err(error) => return Err(error),
            Ok(_) => return Err(ProbeError::new("DIGEST_MISMATCH_ACCEPTED", "assertion")),
        }
        let counts = self.artifact_counts(&admission).await?;
        ensure(counts == (0, 0, 0), "CORRUPT_OBJECT_REFERENCED")?;
        Ok(json!({"rejection": "S3_DIGEST_MISMATCH", "counts": counts}))
    }

    async fn check_object_database_rollback(&self) -> ProbeResult<Value> {
        let (admission, receipt) = self.fixture().await?;
        let verified = self.stored_object(&admission, &receipt).await?;
        let mut tx = self.transaction().await?;
        let artifact_id = self
            .register_ready(&mut tx, &admission, &receipt, &verified)
            .await?;
        self.reference_and_publish(&mut tx, &admission, &receipt, &artifact_id)
            .await?;
        ensure(
            self.artifact_counts(&admission).await? == (0, 0, 0),
            "UNCOMMITTED_PUBLICATION_VISIBLE",
        )?;
        let failure = sqlx::query("SELECT 1 / 0").execute(&mut *tx).await;
        tx.rollback().await?;
        expect_sqlstate(failure, "22012")?;
        let counts = self.artifact_counts(&admission).await?;
        ensure(counts == (0, 0, 0), "DB_ROLLBACK_PUBLISHED_OBJECT")?;
        let head = timeout(IO_TIMEOUT, self.store.head(&verified.key))
            .await
            .map_err(|_| ProbeError::new("S3_TIMEOUT", "s3_orphan_head"))?
            .map_err(|error| storage_error(error, "s3_orphan_head"))?;
        ensure(
            head.size == verified.size_bytes as u64,
            "ORPHAN_OBJECT_NOT_OBSERVED",
        )?;
        Ok(
            json!({"counts": counts, "uploaded_orphan_observed": true, "cleanup_registered": true, "injected_sqlstate": "22012"}),
        )
    }

    async fn delete_recorded_object(&self, intent: &ObjectIntent) -> ProbeResult<()> {
        ensure(
            self.scope.owns_key(&intent.key),
            "CLEANUP_OBJECT_OUTSIDE_PREFIX",
        )?;
        let delete = timeout(IO_TIMEOUT, self.store.delete(&intent.key))
            .await
            .map_err(|_| ProbeError::new("S3_TIMEOUT", "cleanup_delete"))?;
        match delete {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
            Err(error) => return Err(storage_error(error, "cleanup_delete")),
        }
        let head = timeout(IO_TIMEOUT, self.store.head(&intent.key))
            .await
            .map_err(|_| ProbeError::new("S3_TIMEOUT", "cleanup_head"))?;
        match head {
            Err(object_store::Error::NotFound { .. }) => {}
            Err(error) => return Err(storage_error(error, "cleanup_head")),
            Ok(_) => return Err(ProbeError::new("OBJECT_STILL_EXISTS", "cleanup_head")),
        }
        // A timed-out PUT may still complete remotely after our DELETE/HEAD.
        // Attempt cleanup, but retain the exact key for reconciliation and fail.
        ensure(
            intent.put_outcome != PutOutcome::Unknown,
            "S3_WRITE_OUTCOME_UNKNOWN",
        )
    }

    async fn drop_owned_schema(&self) -> ProbeResult<()> {
        match self.schema_owner().await? {
            None => {
                let inventory = self.inventory();
                return ensure(
                    !inventory.schema_commit_started || inventory.schema_commit_acknowledged,
                    "PG_SCHEMA_COMMIT_OUTCOME_UNKNOWN",
                );
            }
            Some(marker) if marker == self.scope.owner_marker => {}
            Some(_) => {
                return Err(ProbeError::new(
                    "SCHEMA_OWNERSHIP_UNVERIFIED",
                    "cleanup_schema",
                ));
            }
        }
        let s = self.scope.schema();
        let mut tx = self.transaction().await?;
        // No CASCADE: outside dependencies cause a visible cleanup failure,
        // rather than deleting any object outside this experiment's inventory.
        sqlx::raw_sql(&format!(
            r#"
            DROP TABLE IF EXISTS {s}.publications, {s}.evidence_references,
                {s}.artifacts, {s}.score_revisions, {s}.idempotency_receipts,
                {s}.jobs, {s}.runs, {s}.plan_revisions;
            DROP SCHEMA {s} RESTRICT;
        "#
        ))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        ensure(self.schema_owner().await?.is_none(), "SCHEMA_STILL_EXISTS")
    }

    async fn cleanup(self: &Arc<Self>) {
        let (schema_attempted, objects) = {
            let inventory = self.inventory();
            (inventory.schema_attempted, inventory.objects.clone())
        };
        for intent in &objects {
            let cleanup_probe = Arc::clone(self);
            let recorded_intent = intent.clone();
            let outcome = tokio::task::spawn_local(async move {
                cleanup_probe.delete_recorded_object(&recorded_intent).await
            })
            .await
            .unwrap_or_else(|_| Err(ProbeError::new("CLEANUP_PANICKED", "cleanup_object")));
            match outcome {
                Ok(()) => self.reporter.emit(
                    "cleanup_object",
                    "pass",
                    json!({"object_key": intent.key.as_ref(), "absent_after_delete": true}),
                ),
                Err(error) => self.reporter.emit(
                    "cleanup_object",
                    "fail",
                    json!({
                        "code": error.code, "stage": error.stage, "object_key": intent.key.as_ref(),
                        "put_outcome": intent.put_outcome, "needs_reconciliation": true,
                    }),
                ),
            }
        }
        if schema_attempted {
            let cleanup_probe = Arc::clone(self);
            let outcome = tokio::task::spawn_local(async move {
                timeout(CHECK_TIMEOUT, cleanup_probe.drop_owned_schema())
                    .await
                    .map_err(|_| ProbeError::new("CLEANUP_TIMEOUT", "cleanup_schema"))?
            })
            .await
            .unwrap_or_else(|_| Err(ProbeError::new("CLEANUP_PANICKED", "cleanup_schema")));
            match outcome {
                Ok(()) => self.reporter.emit(
                    "cleanup_schema",
                    "pass",
                    json!({"schema": self.scope.schema_name, "absent": true}),
                ),
                Err(error) => self.reporter.emit(
                    "cleanup_schema",
                    "fail",
                    json!({
                        "code": error.code, "stage": error.stage, "schema": self.scope.schema_name,
                        "ownership_marker": self.scope.owner_marker, "needs_reconciliation": true,
                    }),
                ),
            }
        }
        if timeout(IO_TIMEOUT, self.pool.close()).await.is_err() {
            self.reporter.error(
                "cleanup_pool",
                ProbeError::new("PG_POOL_CLOSE_TIMEOUT", "cleanup"),
            );
        }
    }
}

#[derive(Clone)]
struct Admission {
    tenant_id: String,
    plan_id: String,
    key: String,
    fingerprint: String,
}

struct Receipt {
    run_id: String,
    job_id: String,
    response: Value,
    created: bool,
    session_id: i32,
}

enum AdmissionMode {
    Normal,
    ObserveUncommitted,
    FaultAfterWrites,
}
enum Rejection {
    OldGeneration,
    Cancelled,
    ExpiredLease,
    WrongOwner,
    ChangedPlanRevision,
    ChangedInputRevision,
}

#[derive(Clone, Copy)]
struct ScoreSubmission {
    generation: i64,
    plan_revision: i64,
    input_revision: i64,
    owner: &'static str,
    score_hundredths: i32,
}

impl ScoreSubmission {
    fn baseline() -> Self {
        Self {
            generation: 1,
            plan_revision: 1,
            input_revision: 1,
            owner: "r1-worker",
            score_hundredths: 8250,
        }
    }
}

struct VerifiedObject {
    key: ObjectPath,
    digest: String,
    size_bytes: i64,
}

async fn supervise() -> bool {
    let scope = Scope::new();
    let reporter = Arc::new(Reporter::new(scope.probe_id.clone()));
    let probe = match Probe::from_env(scope, Arc::clone(&reporter)) {
        Ok(probe) => Arc::new(probe),
        Err(error) => {
            reporter.error("configuration", error);
            reporter.emit(
                "summary",
                "fail",
                json!({"live_checks_executed": false, "cleanup_required": false}),
            );
            return false;
        }
    };
    #[cfg(unix)]
    let signals = (
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()),
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()),
    );
    #[cfg(unix)]
    let (mut interrupt, mut terminate) = match signals {
        (Ok(interrupt), Ok(terminate)) => (interrupt, terminate),
        _ => {
            reporter.error(
                "signal_setup",
                ProbeError::new("SIGNAL_SETUP_FAILED", "startup"),
            );
            probe.cleanup().await;
            return false;
        }
    };
    reporter.emit(
        "start",
        "info",
        json!({"live": true, "max_pg_connections": 6, "concurrent_admissions": 4, "s3_retries": 0}),
    );
    let worker_probe = Arc::clone(&probe);
    let mut worker = tokio::task::spawn_local(async move { worker_probe.execute_checks().await });
    #[cfg(unix)]
    let result = tokio::select! {
        result = &mut worker => result,
        _ = interrupt.recv() => {
            probe.stopping.store(true, Ordering::Relaxed);
            reporter.error("interrupted", ProbeError::new("SIGINT_RECEIVED", "execution"));
            worker.await
        }
        _ = terminate.recv() => {
            probe.stopping.store(true, Ordering::Relaxed);
            reporter.error("interrupted", ProbeError::new("SIGTERM_RECEIVED", "execution"));
            worker.await
        }
    };
    #[cfg(not(unix))]
    let result = (&mut worker).await;
    if result.is_err() {
        reporter.error(
            "probe_worker",
            ProbeError::new("PROBE_PANICKED_OR_CANCELLED", "execution"),
        );
    }
    reporter.report_unfinished_checks();
    probe.cleanup().await;
    let succeeded = reporter.succeeded();
    reporter.emit(
        "summary",
        if succeeded { "pass" } else { "fail" },
        json!({
            "failed_checks": reporter.failures.load(Ordering::Relaxed),
            "skipped_checks": reporter.skipped.load(Ordering::Relaxed),
            "output_failed": reporter.output_failed.load(Ordering::Relaxed),
            "live_checks_executed": true,
            "production_api_validated": false, "capacity_validated": false,
        }),
    );
    succeeded && !reporter.output_failed.load(Ordering::Relaxed)
}

fn entry() -> bool {
    if !live_allowed(env::var_os("R1_ALLOW_LIVE_PROBES").as_deref()) {
        let _ = writeln!(
            io::stdout().lock(),
            "{{\"probe\":\"r1_storage\",\"check\":\"live_gate\",\"status\":\"fail\",\"code\":\"LIVE_PROBES_NOT_ENABLED\",\"live_checks_executed\":false}}"
        );
        return false;
    }
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        // SQLx raw SQL borrows stay on one thread; independent connections still
        // execute concurrently. Local tasks preserve panic isolation for cleanup.
        Ok(runtime) => tokio::task::LocalSet::new().block_on(&runtime, supervise()),
        Err(_) => {
            let _ = writeln!(
                io::stderr().lock(),
                "{{\"probe\":\"r1_storage\",\"status\":\"fail\",\"code\":\"RUNTIME_INIT_FAILED\"}}"
            );
            false
        }
    }
}

fn main() -> ExitCode {
    // Dependency panic messages may embed connection information. The worker's
    // JoinError is classified without formatting it, then cleanup still runs.
    std::panic::set_hook(Box::new(|_| {}));
    match std::panic::catch_unwind(entry) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(_) => {
            let _ = writeln!(
                io::stderr().lock(),
                "{{\"probe\":\"r1_storage\",\"status\":\"fail\",\"code\":\"SUPERVISOR_PANIC_CLEANUP_UNCONFIRMED\"}}"
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cleanup_preflight_verifies_written_object_and_preserves_other_data() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let other_key = ObjectPath::from("unrelated/retained.txt");
        store
            .put(&other_key, b"retain".to_vec().into())
            .await
            .unwrap();
        let scope = Scope::new();
        let probe = Probe {
            reporter: Arc::new(Reporter::new(scope.probe_id.clone())),
            scope,
            pool: PgPoolOptions::new().connect_lazy_with(PgConnectOptions::new()),
            store: store.clone(),
            inventory: Mutex::new(CleanupInventory::default()),
            stopping: AtomicBool::new(false),
        };
        probe.check_cleanup_access().await.unwrap();
        let probe_key = {
            let inventory = probe.inventory();
            assert_eq!(inventory.objects.len(), 1);
            assert!(inventory.objects[0].put_outcome == PutOutcome::Acknowledged);
            inventory.objects[0].key.clone()
        };
        assert_eq!(
            store
                .get(&other_key)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
                .as_ref(),
            b"retain"
        );
        assert!(matches!(
            store.head(&probe_key).await,
            Err(object_store::Error::NotFound { .. })
        ));
    }

    #[test]
    fn live_gate_requires_exact_opt_in() {
        assert!(live_allowed(Some(OsStr::new("1"))));
        for value in [
            None,
            Some(""),
            Some("true"),
            Some("0"),
            Some(" 1"),
            Some("1\n"),
        ] {
            assert!(!live_allowed(value.map(OsStr::new)));
        }
    }

    #[test]
    fn generated_scope_is_random_and_cannot_adopt_another_scope() {
        let a = Scope::new();
        let b = Scope::new();
        assert_ne!(a.schema_name, b.schema_name);
        assert_ne!(a.owner_marker, b.owner_marker);
        assert_eq!(a.schema_name.len(), 35);
        assert!(a.schema_name.starts_with("r1_"));
        assert!(
            a.schema_name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        );
        assert_eq!(Uuid::parse_str(&a.probe_id).unwrap().get_version_num(), 4);
        let tenant = Uuid::new_v4().to_string();
        let run = Uuid::new_v4().to_string();
        let key = a.object_key(&tenant, &run, &sha256(b"test")).unwrap();
        assert!(a.owns_key(&key));
        assert!(!b.owns_key(&key));
        assert!(!a.owns_key(&ObjectPath::from(format!("{}-other/file", a.object_prefix))));
        assert!(a.object_key("../outside", &run, &sha256(b"test")).is_err());
    }

    #[test]
    fn sha256_uses_known_digest() {
        assert_eq!(
            sha256(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn endpoint_validation_does_not_accept_credentials_or_insecure_transport() {
        assert!(validate_endpoint("https://internal.example.invalid").is_ok());
        for value in [
            "http://internal.example.invalid",
            "https://user:secret@example.invalid",
            "https://example.invalid?signature=secret",
            "https://example.invalid#secret",
            "https://example.invalid:0",
            "invalid",
        ] {
            assert!(validate_endpoint(value).is_err());
        }
    }

    #[test]
    fn configured_referer_is_optional_but_must_not_contain_credentials() {
        assert!(storage_client_options(None).is_ok());
        assert!(storage_client_options(Some("https://platform.example.invalid/")).is_ok());
        for value in [
            "http://platform.example.invalid/",
            "https://user:secret@example.invalid/",
            "https://example.invalid/?token=secret",
            "https://example.invalid/\r\nHeader: injected",
        ] {
            assert!(storage_client_options(Some(value)).is_err());
        }
    }

    #[test]
    fn errors_discard_provider_messages_and_distinguish_missing_from_denied() {
        let secret = "https://private.invalid/?secret=do-not-log";
        let source =
            || Box::new(io::Error::other(secret)) as Box<dyn std::error::Error + Send + Sync>;
        let missing = storage_error(
            object_store::Error::NotFound {
                path: secret.into(),
                source: source(),
            },
            "head",
        );
        let denied = storage_error(
            object_store::Error::PermissionDenied {
                path: secret.into(),
                source: source(),
            },
            "head",
        );
        let database = ProbeError::from(sqlx::Error::Io(io::Error::other(secret)));
        assert_eq!(missing.code, "S3_NOT_FOUND");
        assert_eq!(denied.code, "S3_PERMISSION_DENIED");
        assert_eq!(database.code, "PG_CONNECTION_ERROR");
        assert!(!format!("{missing:?}{denied:?}{database:?}").contains("private.invalid"));
        assert!(expect_sqlstate(Ok::<_, sqlx::Error>(()), "23503").is_err());
    }
}
