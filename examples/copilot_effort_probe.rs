//! Probes what the installed Copilot CLI lists as a Model's reasoning efforts against what it
//! accepts when a Session is switched onto that Model with one — the switch Suru makes at the
//! start of a Turn selected under an effort.
//!
//! Drives the CLI through the SDK the way Suru's Copilot Provider does. The default run lists the
//! Model from both catalogs, switches a Session onto it with every listed effort, and opens one
//! with `none` up front; each `PROBE_*` variable adds a further round:
//!
//! ```sh
//! cargo run --example copilot_effort_probe -- gpt-6-luna
//! PROBE_ALL=1  cargo run --example copilot_effort_probe   # a cold switch to `none` on every Model
//! PROBE_SEQ=1  cargo run --example copilot_effort_probe   # `none` after another switch first
//! PROBE_ISO=1  cargo run --example copilot_effort_probe   # what makes the CLI accept `none`
//! PROBE_TURN=1 cargo run --example copilot_effort_probe   # a Turn at `none` once it is accepted
//! ```
//!
//! `SURU_COPILOT_PATH` names the CLI (default `copilot`). What the probes showed is recorded in
//! `docs/validation/copilot-cold-switch-effort.md`.
use std::path::PathBuf;

use github_copilot_sdk::{
    CliProgram, Client, ClientOptions,
    types::{SessionConfig, SetModelOptions},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let model = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "gpt-6-luna".to_owned());
    let program = std::env::var_os("SURU_COPILOT_PATH").unwrap_or_else(|| "copilot".into());
    let client =
        Client::start(ClientOptions::new().with_program(CliProgram::Path(PathBuf::from(program))))
            .await?;

    let flat = client.rpc().models().list().await?.models;
    let listed = flat
        .iter()
        .find(|m| m.id == model)
        .ok_or("model not in models.list")?;
    println!(
        "[PROBE] models.list        {model}: supported={:?} default={:?}",
        listed.supported_reasoning_efforts, listed.default_reasoning_effort
    );

    let session = client
        .create_session(SessionConfig::default().with_model("gpt-5-mini"))
        .await?;
    let per_session = session.rpc().model().list().await?.list;
    if let Some(m) = per_session.iter().find(|m| m["id"] == model) {
        println!(
            "[PROBE] session.model.list {model}: supported={} default={}",
            m["supportedReasoningEfforts"], m["defaultReasoningEffort"]
        );
    } else {
        println!("[PROBE] session.model.list does not carry {model}");
    }

    let no_options: Option<SetModelOptions> = None;
    println!(
        "[PROBE] switchTo({model}) no effort   -> {:?}",
        session.set_model(&model, no_options).await.map(|_| ())
    );
    println!(
        "[PROBE] current after no effort     -> {:?}",
        current(&session).await
    );
    for effort in listed
        .supported_reasoning_efforts
        .clone()
        .unwrap_or_default()
    {
        let opts = SetModelOptions {
            reasoning_effort: Some(effort.clone()),
            ..Default::default()
        };
        println!(
            "[PROBE] switchTo({model}, effort={effort:?}) -> {:?}",
            session.set_model(&model, Some(opts)).await.map(|_| ())
        );
    }
    println!(
        "[PROBE] current at end              -> {:?}",
        current(&session).await
    );

    // A fresh Session opened on the Model with "none" chosen up front, as an Errand would open it.
    let opened = client
        .create_session(
            SessionConfig::default()
                .with_model(&model)
                .with_reasoning_effort("none"),
        )
        .await;
    match opened {
        Ok(s) => println!(
            "[PROBE] create_session(effort=none) -> Ok, current={:?}",
            current(&s).await
        ),
        Err(e) => println!("[PROBE] create_session(effort=none) -> Err({e})"),
    }
    // Exactly Suru's own path: a Session opened on no Model, then switched onto the Model with
    // the effort and the context tier lowered together, as `apply_selection` does.
    use github_copilot_sdk::session_events::ContextTier;
    for (effort, tier) in [
        (Some("none"), None),
        (Some("none"), Some(ContextTier::Default)),
        (Some("low"), Some(ContextTier::Default)),
        (None, Some(ContextTier::Default)),
    ] {
        let fresh = client.create_session(SessionConfig::default()).await?;
        println!(
            "[PROBE] fresh current                -> {:?}",
            current(&fresh).await
        );
        let opts = SetModelOptions {
            reasoning_effort: effort.map(str::to_owned),
            context_tier: tier.clone(),
            ..Default::default()
        };
        println!(
            "[PROBE] fresh switchTo({model}, effort={effort:?}, tier={tier:?}) -> {:?}",
            fresh.set_model(&model, Some(opts)).await.map(|_| ())
        );
    }
    if std::env::var_os("PROBE_ALL").is_some() {
        for m in flat
            .iter()
            .filter(|m| m.supported_reasoning_efforts.is_some())
        {
            let fresh = client.create_session(SessionConfig::default()).await?;
            let opts = SetModelOptions {
                reasoning_effort: Some("none".to_owned()),
                ..Default::default()
            };
            let verdict = match fresh.set_model(&m.id, Some(opts)).await {
                Ok(()) => "accepted".to_owned(),
                Err(e) => format!("REFUSED: {e}"),
            };
            println!(
                "[PROBE-ALL] {:<28} supported={:?} default={:?} switchTo(none) -> {verdict}",
                m.id, m.supported_reasoning_efforts, m.default_reasoning_effort
            );
        }
    }
    if std::env::var_os("PROBE_SEQ").is_some() {
        // Each sequence: (first switch: model, effort) then (second switch: model, effort).
        let sequences: &[(&str, Option<&str>, &str, Option<&str>)] = &[
            ("gpt-5-mini", Some("low"), "gpt-6-luna", Some("none")),
            ("gpt-6-luna", Some("low"), "gpt-6-luna", Some("none")),
            ("gpt-6-luna", Some("low"), "gpt-5.4", Some("none")),
            ("gpt-6-luna", Some("low"), "gpt-5-mini", Some("none")),
            ("gpt-6-luna", None, "gpt-6-luna", Some("bogus")),
            ("gpt-6-luna", None, "gpt-6-luna", Some("none")),
        ];
        for (m1, e1, m2, e2) in sequences {
            let fresh = client.create_session(SessionConfig::default()).await?;
            let first = fresh
                .set_model(
                    m1,
                    Some(SetModelOptions {
                        reasoning_effort: e1.map(str::to_owned),
                        ..Default::default()
                    }),
                )
                .await
                .map(|_| ());
            let second = fresh
                .set_model(
                    m2,
                    Some(SetModelOptions {
                        reasoning_effort: e2.map(str::to_owned),
                        ..Default::default()
                    }),
                )
                .await
                .map_err(|e| e.to_string());
            println!(
                "[PROBE-SEQ] ({m1},{e1:?}) -> {first:?}; then ({m2},{e2:?}) -> {:?}; current={}",
                second,
                current(&fresh).await
            );
        }
    }
    if std::env::var_os("PROBE_ISO").is_some() {
        let none = || {
            Some(SetModelOptions {
                reasoning_effort: Some("none".to_owned()),
                ..Default::default()
            })
        };
        let show = |label: &str, r: Result<(), github_copilot_sdk::Error>| {
            println!(
                "[PROBE-ISO] {label} -> {}",
                match r {
                    Ok(()) => "accepted".to_owned(),
                    Err(e) => format!("REFUSED {e}"),
                }
            );
        };
        let s1 = client
            .create_session(SessionConfig::default().with_model("gpt-5-mini"))
            .await?;
        show(
            "S1 created(gpt-5-mini); switchTo(luna,none)",
            s1.set_model(&model, none()).await,
        );
        let s2 = client
            .create_session(SessionConfig::default().with_model("gpt-5-mini"))
            .await?;
        let _ = s2.set_model(&model, None).await;
        show(
            "S2 created(gpt-5-mini); switchTo(luna); switchTo(luna,none)",
            s2.set_model(&model, none()).await,
        );
        let s3 = client.create_session(SessionConfig::default()).await?;
        let listed = s3.rpc().model().list().await?.list;
        if let Some(m) = listed.iter().find(|m| m["id"] == model) {
            println!(
                "[PROBE-ISO] session.model.list raw: {}",
                serde_json::to_string(m)?
            );
        }
        show(
            "S3 created(); session.model.list; switchTo(luna,none)",
            s3.set_model(&model, none()).await,
        );
        let s4 = client
            .create_session(SessionConfig::default().with_model(&model))
            .await?;
        show(
            "S4 created(luna); switchTo(luna,none)",
            s4.set_model(&model, none()).await,
        );
        let s5 = client.create_session(SessionConfig::default()).await?;
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        show(
            "S5 created(); sleep 3s; switchTo(luna,none)",
            s5.set_model(&model, none()).await,
        );
        let s6 = client
            .create_session(SessionConfig::default().with_model("gpt-5-mini"))
            .await?;
        let _ = s6.set_model("gpt-5-mini", None).await;
        show(
            "S6 created(gpt-5-mini); switchTo(gpt-5-mini); switchTo(luna,none)",
            s6.set_model(&model, none()).await,
        );
    }
    if std::env::var_os("PROBE_TURN").is_some() {
        use github_copilot_sdk::MessageOptions;
        let s = client.create_session(SessionConfig::default()).await?;
        let started = std::time::Instant::now();
        let _ = s.rpc().model().list().await?;
        println!(
            "[PROBE-TURN] session.model.list took {:?}",
            started.elapsed()
        );
        let opts = SetModelOptions {
            reasoning_effort: Some("none".to_owned()),
            ..Default::default()
        };
        println!(
            "[PROBE-TURN] switchTo(luna,none) -> {:?}",
            s.set_model(&model, Some(opts)).await.map(|_| ())
        );
        println!("[PROBE-TURN] current -> {}", current(&s).await);
        let reply = s
            .send_and_wait(MessageOptions::new(
                "Reply with exactly the single word OK and nothing else.",
            ))
            .await;
        match reply {
            Ok(ev) => println!(
                "[PROBE-TURN] turn -> Ok, last event: {}",
                serde_json::to_string(&ev)?
                    .chars()
                    .take(400)
                    .collect::<String>()
            ),
            Err(e) => println!("[PROBE-TURN] turn -> Err({e})"),
        }
        println!("[PROBE-TURN] current after turn -> {}", current(&s).await);
    }
    client.stop().await?;
    Ok(())
}

async fn current(session: &github_copilot_sdk::session::Session) -> String {
    match session.rpc().model().get_current().await {
        Ok(c) => format!("model={:?} effort={:?}", c.model_id, c.reasoning_effort),
        Err(e) => format!("Err({e})"),
    }
}
