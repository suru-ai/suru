//! Explicit populated-history benchmark. Run each case in a fresh process:
//! cargo run --release --example storage_startup_perf -- 100 100 4096
//! Arguments: Sessions, Turns per Session, bytes per Message/Activity.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
#[path = "../tests/support/failing_provider.rs"]
mod failing_provider;
use diesel::{connection::SimpleConnection, prelude::*, sql_types::Text};
use suru::{
    protocol::{SessionId, SessionSnapshot},
    server::ServerConfig,
};
use tracing_subscriber::{layer::Context, prelude::*};

#[derive(Clone, Default)]
struct Measurements {
    queries: Arc<AtomicU64>,
    decode_us: Arc<AtomicU64>,
}
impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Measurements {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        struct Fields<'a>(&'a Measurements);
        impl tracing::field::Visit for Fields<'_> {
            fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                match field.name() {
                    "storage_query" => {
                        self.0.queries.fetch_add(value, Ordering::Relaxed);
                    }
                    "storage_decode_us" => {
                        self.0.decode_us.fetch_add(value, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
            fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
        }
        event.record(&mut Fields(self));
    }
}
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    anyhow::ensure!(
        args.len() == 3 || args.len() == 6,
        "expected Sessions Turns bytes"
    );
    let sizes = args[..3]
        .iter()
        .map(|a| a.to_str().unwrap().parse::<usize>())
        .collect::<Result<Vec<_>, _>>()?;
    let [count, turns, bytes] = sizes.as_slice() else {
        unreachable!()
    };
    let measuring = args.len() == 6 && args[3] == "--measure";
    let temporary = if measuring {
        None
    } else {
        Some(tempfile::tempdir()?)
    };
    let root = temporary
        .as_ref()
        .map(|root| root.path().to_owned())
        .unwrap_or_else(|| std::path::PathBuf::from(&args[4]));
    let config = ServerConfig::new(root.join("state"), "storage-benchmark")?
        .with_data_dir(root.join("data"));
    if !measuring {
        let id = prepare(&config, &root, count, turns, bytes).await?;
        // Fixture creation warms filesystem caches but never the measured
        // process's allocator, runtime, SQLite connections, or server setup.
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(&args[..3])
            .arg("--measure")
            .arg(&root)
            .arg(id.to_string())
            .status()?;
        anyhow::ensure!(status.success(), "measurement process failed: {status}");
        return Ok(());
    }
    let id = SessionId::from_uuid(uuid::Uuid::parse_str(args[5].to_str().unwrap())?);
    let metrics = Measurements::default();
    tracing_subscriber::registry().with(metrics.clone()).init();
    let running = Arc::new(AtomicBool::new(true));
    let peak = Arc::new(AtomicU64::new(0));
    let sampler = {
        let running = running.clone();
        let peak = peak.clone();
        std::thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                peak.fetch_max(memory(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let before = memory();
    let start = Instant::now();
    let server = failing_provider::spawn_with_failing_provider(config).await?;
    let startup = start.elapsed();
    let steady = memory();
    running.store(false, Ordering::Relaxed);
    sampler.join().unwrap();
    let queries = metrics.queries.load(Ordering::Relaxed);
    let decode = metrics.decode_us.load(Ordering::Relaxed);
    let start = Instant::now();
    if *count > 0 {
        let snapshot = reqwest::Client::new()
            .get(format!("{}/v1/sessions/{id}", server.descriptor().base_url))
            .bearer_auth(&server.descriptor().token)
            .send()
            .await?
            .error_for_status()?
            .json::<SessionSnapshot>()
            .await?;
        anyhow::ensure!(
            snapshot.messages.len() == *turns,
            "fixture must be readable"
        );
    }
    println!(
        "sessions={count} turns={turns} bytes={bytes} startup_ms={:.2} queries={queries} decode_ms={:.2} before_mib={:.2} peak_mib={:.2} steady_mib={:.2} first_open_ms={:.2}",
        startup.as_secs_f64() * 1000.0,
        decode as f64 / 1000.0,
        before as f64 / 1048576.0,
        peak.load(Ordering::Relaxed).max(steady) as f64 / 1048576.0,
        steady as f64 / 1048576.0,
        start.elapsed().as_secs_f64() * 1000.0
    );
    server.shutdown().await?;
    Ok(())
}
async fn prepare(
    config: &ServerConfig,
    root: &std::path::Path,
    count: &usize,
    turns: &usize,
    bytes: &usize,
) -> anyhow::Result<SessionId> {
    let server = failing_provider::spawn_with_failing_provider(config.clone()).await?;
    server.shutdown().await?;
    let id = SessionId::new();
    let mut db = SqliteConnection::establish(config.data_dir().join("suru.db").to_str().unwrap())?;
    db.batch_execute("PRAGMA foreign_keys = ON;")?;
    let workspace = serde_json::json!({"path":root}).to_string();
    let content = "x".repeat(*bytes);
    let message = serde_json::json!({"role":"agent","status":"completed","content":content,"truncated":false}).to_string();
    let activity = serde_json::json!({"kind":"reasoning","status":"completed","title":null,"content":content,"content_truncated":false,"duration_ms":null}).to_string();
    db.transaction::<_, anyhow::Error, _>(|db| {
        for s in 0..*count {
            let sid = if s == 0 { id } else { SessionId::new() }.to_string();
            diesel::sql_query("INSERT INTO sessions(id,title,created_at,updated_at,workspace,agent_selection_availability,status,revision) VALUES (?, 'Benchmark',1,2,?, '\"available\"','\"idle\"',1)")
                .bind::<Text,_>(&sid).bind::<Text,_>(&workspace).execute(db)?;
            for t in 0..*turns {
                let tid = uuid::Uuid::new_v4().to_string();
                diesel::sql_query("INSERT INTO turns(id,session_id,row_order,payload) VALUES (?,?,?,?)")
                    .bind::<Text,_>(&tid).bind::<Text,_>(&sid).bind::<diesel::sql_types::BigInt,_>(t as i64)
                    .bind::<Text,_>(r#"{"agent":null,"status":"completed","started_at":1,"settled_at":2}"#).execute(db)?;
                for (table, payload, order) in [("messages", &message, t * 2), ("activities", &activity, t * 2 + 1)] {
                    diesel::sql_query(format!("INSERT INTO {table}(id,session_id,turn_id,row_order,transcript_order,payload) VALUES (?,?,?,?,?,?)"))
                        .bind::<Text,_>(uuid::Uuid::new_v4().to_string()).bind::<Text,_>(&sid).bind::<Text,_>(&tid)
                        .bind::<diesel::sql_types::BigInt,_>(t as i64).bind::<diesel::sql_types::BigInt,_>(order as i64).bind::<Text,_>(payload).execute(db)?;
                }
            }
        }
        Ok(())
    })?;
    drop(db);
    Ok(id)
}
fn memory() -> u64 {
    let pid = sysinfo::get_current_pid().unwrap();
    let mut system = sysinfo::System::new();
    system.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[pid]),
        true,
        sysinfo::ProcessRefreshKind::nothing().with_memory(),
    );
    system.process(pid).map_or(0, sysinfo::Process::memory)
}
