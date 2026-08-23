use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use recipedia::config::Config;
use recipedia::db;
use recipedia::sync;
use recipedia::web::{router, AppState, SharedState};

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // `recipedia-server token <label>` mints a publish token and exits.
        Some("token") => {
            let label = args
                .get(1)
                .context("usage: recipedia-server token <label>")?;
            let config = Config::from_env();
            let conn = db::open(&config.db_path)?;
            let token = db::token_create(&conn, label)?;
            println!("{token}");
            println!("(store it now — only its SHA-256 is kept)");
            Ok(())
        }
        _ => serve().await,
    }
}

async fn serve() -> Result<()> {
    let config = Config::from_env();
    std::fs::create_dir_all(&config.state_dir)?;
    std::fs::create_dir_all(&config.repo_dir)?;
    let conn = db::open(&config.db_path)?;
    let state: SharedState = Arc::new(AppState {
        db: Mutex::new(conn),
        config: config.clone(),
        syncing: AtomicBool::new(false),
    });

    // Initial sync so the site is never empty on first boot; failures are
    // non-fatal (the poll loop and webhooks will catch up).
    {
        let st = state.clone();
        let trigger = "boot".to_string();
        tokio::task::spawn_blocking(move || {
            let conn = st.db.lock().expect("boot sync: db mutex poisoned");
            match sync::run_sync(&conn, &st.config, &trigger) {
                Ok(count) => println!("boot sync: {count} recipes"),
                Err(err) => eprintln!("boot sync failed: {err:#}"),
            }
        });
    }

    // Fallback poll: cheap remote-HEAD probe per tree, full sync only on change.
    {
        let st = state.clone();
        let poll_secs = config.poll_secs;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(poll_secs));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            ticker.tick().await; // first tick fires immediately; boot sync covers it
            loop {
                ticker.tick().await;
                if st.syncing.load(std::sync::atomic::Ordering::SeqCst) {
                    continue;
                }
                // Probe every tree; any change (or an unknown HEAD) syncs
                // the whole combined world in one pass.
                let probes = {
                    let st = st.clone();
                    tokio::task::spawn_blocking(move || {
                        st.config
                            .git_sources
                            .iter()
                            .map(|s| {
                                let stored = {
                                    let conn = st.db.lock().expect("poll: db mutex poisoned");
                                    db::meta_get(&conn, &format!("last_commit:{}", s.arch))
                                        .ok()
                                        .flatten()
                                        .unwrap_or_default()
                                };
                                (s.arch.clone(), s.url.clone(), sync::remote_head(&s.url), stored)
                            })
                            .collect::<Vec<_>>()
                    })
                    .await
                };
                let probes = match probes {
                    Ok(p) => p,
                    Err(err) => {
                        eprintln!("poll: probe task failed: {err}");
                        continue;
                    }
                };
                let mut changed = false;
                for (arch, url, remote, stored) in &probes {
                    match remote {
                        Ok(sha) if sha == stored => {}
                        Ok(sha) => {
                            changed = true;
                            println!("poll: {arch} remote moved to {sha}, syncing");
                        }
                        Err(err) => eprintln!("poll: {arch} probe failed: {err:#} ({url})"),
                    }
                }
                if !changed {
                    continue;
                }
                recipedia::web::trigger_sync(st.clone(), "poll").await;
            }
        });
    }

    let app = router(state);
    let listener = tokio::net::TcpListener::bind(&config.listen)
        .await
        .with_context(|| format!("binding {}", config.listen))?;
    println!("recipedia listening on http://{}", config.listen);
    axum::serve(listener, app).await?;
    Ok(())
}
