//! Background-spawn hooks and the check-baseline refresher.
//!
//! Extracted from `engine/mod.rs`.

use super::*;

impl KodEngine {
    pub fn build_background_hook(&self) -> kod_tools::context::BackgroundSpawnHook {
        let steers = std::sync::Arc::clone(&self.steers);
        let runner = std::sync::Arc::clone(&self.background);
        let working_dir = self.working_dir.clone();
        // Delta §11.4: the delivery queue the completion closure
        // enqueues into.
        let async_delivery = std::sync::Arc::clone(&self.async_delivery);

        kod_tools::context::BackgroundSpawnHook::new(
            move |command: &str, stall: Option<u64>, holder: &str| -> Option<String> {
                let job = runner.allocate_id();
                let id_str = job.0.to_string();

                let dir = dirs::home_dir()
                    .map(|h| h.join(".kod").join("background"))
                    .unwrap_or_else(std::env::temp_dir);
                sweep_background_spools(&dir);
                let spool_path = dir.join(format!("{}.log", job.0));

                let mut spool = match crate::output_spool::OutputSpool::create(&spool_path) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(error = %e, "background spool create failed");
                        return None;
                    }
                };

                runner.register(
                    job,
                    crate::background::JobKind::Shell {
                        command: command.chars().take(120).collect(),
                        spool: spool_path.clone(),
                    },
                );

                let full = format!("{command} 2>&1");
                let mut cmd = if cfg!(windows) {
                    let mut c = tokio::process::Command::new("cmd");
                    c.arg("/C").arg(&full);
                    c
                } else {
                    let mut c = tokio::process::Command::new("sh");
                    c.arg("-c").arg(&full);
                    c
                };
                cmd.current_dir(&working_dir);
                cmd.stdout(std::process::Stdio::piped());
                cmd.stderr(std::process::Stdio::null());

                let mut child = match cmd.spawn() {
                    Ok(c) => c,
                    Err(e) => {
                        runner.fail(job, format!("spawn failed: {e}"));
                        return None;
                    }
                };

                let Some(mut stdout) = child.stdout.take() else {
                    runner.fail(job, "child had no stdout pipe".to_string());
                    return None;
                };

                let stall_secs = stall.filter(|s| *s > 0).map(|s| s.max(30));
                let holder = holder.to_string();

                // The outer closure is `Fn` — it is called once per
                // background request — so every value the task owns
                // must be cloned out of it. The `Arc`s are cheap; the
                // job id is named in two notices and returned to the
                // caller, so the task takes a copy.
                let steers_task = std::sync::Arc::clone(&steers);
                let runner_task = std::sync::Arc::clone(&runner);
                let delivery_task = std::sync::Arc::clone(&async_delivery);
                let id_task = id_str.clone();

                // Guarded spawn: a panic inside the task becomes a
                // job failure rather than a permanently-Running job
                // whose caller waits forever.
                let runner_for_watch = std::sync::Arc::clone(&runner_task);
                runner_for_watch.spawn_guarded(job, async move {
                    use tokio::io::AsyncReadExt as _;
                    let mut buf = [0u8; 4096];
                    let mut stall_reported = false;
                    let stall_dur = stall_secs.map(std::time::Duration::from_secs);

                    loop {
                        let read = match stall_dur {
                            Some(d) => match tokio::time::timeout(d, stdout.read(&mut buf)).await {
                                Ok(r) => r,
                                Err(_) => {
                                    if !stall_reported {
                                        push_background_interrupt(
                                            &steers_task,
                                            &holder,
                                            format!(
                                                "background job {id_task} has produced no output for {secs}s",
                                                secs = stall_secs.unwrap_or(0),
                                            ),
                                        )
                                        .await;
                                        stall_reported = true;
                                    }
                                    continue;
                                }
                            },
                            None => stdout.read(&mut buf).await,
                        };

                        match read {
                            Ok(0) => break,
                            Ok(n) => {
                                if spool.append(&buf[..n]).is_err() {
                                    break;
                                }
                                stall_reported = false;
                            }
                            Err(_) => break,
                        }
                    }

                    let status = match tokio::time::timeout(
                        std::time::Duration::from_secs(60),
                        child.wait(),
                    )
                    .await
                    {
                        Ok(s) => s,
                        Err(_) => {
                            let _ = child.start_kill();
                            child.wait().await
                        }
                    };
                    let preview = spool.preview();
                    let summary = match status {
                        Ok(s) if s.success() => format!(
                            "completed; {} bytes of output.\n{}",
                            spool.written(),
                            preview,
                        ),
                        Ok(s) => format!(
                            "exited with {s}; {} bytes of output.\n{}",
                            spool.written(),
                            preview,
                        ),
                        Err(e) => format!("could not read exit status: {e}"),
                    };
                    runner_task.complete(job, summary.clone());
                    // Delta §11.4: enqueue into the batched delivery
                    // queue *and* push the immediate interrupt. The
                    // queue batches; the interrupt wakes an idle
                    // turn. A consumer that only wants one of the two
                    // can read the other path's behaviour, but both
                    // firing is the safe default: the queue survives
                    // a drop, the interrupt wakes a sleeping agent.
                    let epoch = delivery_task.lock().epoch();
                    delivery_task.lock().enqueue(
                        crate::async_delivery::AsyncResult {
                            job_id: job.0,
                            owner_id: holder.clone(),
                            kind: "shell".to_string(),
                            body: summary.clone(),
                            artifact: None,
                            epoch,
                        },
                    );
                    push_background_interrupt(
                        &steers_task,
                        &holder,
                        format!("background job {id_task} finished: {summary}"),
                    )
                    .await;
                });

                Some(id_str)
            },
        )
    }

    /// Delta §11.4: build the child-adoption hook. A command that
    /// outlived its deadline and is still running is moved here. The
    /// hook registers a job, spawns a task that drains both pipes
    /// into a spool, reaps the child, and delivers a completion
    /// notice — the same shape as `build_background_hook`, but the
    /// child and its pipes arrive already open rather than being
    /// spawned here.
    pub fn build_background_adopt_hook(&self) -> kod_tools::context::BackgroundAdoptHook {
        let steers = std::sync::Arc::clone(&self.steers);
        let runner = std::sync::Arc::clone(&self.background);
        let async_delivery = std::sync::Arc::clone(&self.async_delivery);

        kod_tools::context::BackgroundAdoptHook::new(move |detached| {
            let job = runner.allocate_id();
            let id_str = job.0.to_string();

            let dir = dirs::home_dir()
                .map(|h| h.join(".kod").join("background"))
                .unwrap_or_else(std::env::temp_dir);
            sweep_background_spools(&dir);
            let spool_path = dir.join(format!("{}.log", job.0));

            let mut spool = match crate::output_spool::OutputSpool::create(&spool_path) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "background spool create failed");
                    // Fall through: still register the job so the
                    // caller sees a running job, but without a spool
                    // the completion notice carries no preview.
                    return None;
                }
            };

            // Write the bytes the tool already read before handing
            // off. Without this the first chunk of output would be
            // lost.
            if !detached.prior_stdout.is_empty() {
                let _ = spool.append(&detached.prior_stdout);
            }
            if !detached.prior_stderr.is_empty() {
                let _ = spool.append(&detached.prior_stderr);
            }

            runner.register(
                job,
                crate::background::JobKind::Shell {
                    command: detached.command.chars().take(120).collect(),
                    spool: spool_path.clone(),
                },
            );

            let holder = detached.holder.clone();
            let steers_task = std::sync::Arc::clone(&steers);
            let runner_task = std::sync::Arc::clone(&runner);
            let delivery_task = std::sync::Arc::clone(&async_delivery);
            let id_task = id_str.clone();

            let mut child = detached.child;
            let mut stdout = detached.stdout;
            let mut stderr = detached.stderr;

            let runner_for_watch = std::sync::Arc::clone(&runner_task);
            runner_for_watch.spawn_guarded(job, async move {
                use tokio::io::AsyncReadExt as _;
                // Two buffers: the two `select!` arms must not borrow
                // the same slice mutably.
                let mut stdout_buf = [0u8; 4096];
                let mut stderr_buf = [0u8; 4096];
                // Drain both pipes concurrently. Either hitting EOF
                // or an error stops its read; the loop exits once
                // both are done.
                let mut stdout_done = false;
                let mut stderr_done = false;
                while !(stdout_done && stderr_done) {
                    tokio::select! {
                        r = stdout.read(&mut stdout_buf), if !stdout_done => {
                            match r {
                                Ok(0) | Err(_) => stdout_done = true,
                                Ok(n) => {
                                    let _ = spool.append(&stdout_buf[..n]);
                                }
                            }
                        }
                        r = stderr.read(&mut stderr_buf), if !stderr_done => {
                            match r {
                                Ok(0) | Err(_) => stderr_done = true,
                                Ok(n) => {
                                    let _ = spool.append(&stderr_buf[..n]);
                                }
                            }
                        }
                    }
                }
                let status =
                    match tokio::time::timeout(std::time::Duration::from_secs(60), child.wait())
                        .await
                    {
                        Ok(s) => s,
                        Err(_) => {
                            let _ = child.start_kill();
                            child.wait().await
                        }
                    };
                let preview = spool.preview();
                let summary = match status {
                    Ok(s) if s.success() => format!(
                        "completed (adopted); {} bytes of output.\n{}",
                        spool.written(),
                        preview,
                    ),
                    Ok(s) => format!(
                        "exited with {s} (adopted); {} bytes of output.\n{}",
                        spool.written(),
                        preview,
                    ),
                    Err(e) => format!("could not read exit status: {e}"),
                };
                runner_task.complete(job, summary.clone());
                let epoch = delivery_task.lock().epoch();
                delivery_task
                    .lock()
                    .enqueue(crate::async_delivery::AsyncResult {
                        job_id: job.0,
                        owner_id: holder.clone(),
                        kind: "shell".to_string(),
                        body: summary.clone(),
                        artifact: None,
                        epoch,
                    });
                push_background_interrupt(
                    &steers_task,
                    &holder,
                    format!("background job {id_task} finished: {summary}"),
                )
                .await;
            });

            Some(id_str)
        })
    }

    /// Install the hook on a per-call context.
    pub(crate) fn install_background_hook(&self, ctx: &mut ToolContext) {
        ctx.on_background_command = Some(self.build_background_hook());
        ctx.on_background_adopt = Some(self.build_background_adopt_hook());
    }
}

/// Small owned snapshot of the parts of `KodEngine` that the baseline
/// refresh needs. Exists because `KodEngine::start` takes `&self` and
/// therefore cannot wrap `self` in an `Arc` for a background task.
pub(crate) struct BaselineRefresher {
    pub(crate) working_dir: std::path::PathBuf,
    pub(crate) check_baseline: Arc<RwLock<Option<Vec<kod_tools::check::Diagnostic>>>>,
}

impl BaselineRefresher {
    pub(crate) async fn refresh_check_baseline(&self) {
        // H-E13: run the check against the transcript's working
        // directory, not the engine's. A swarm agent
        // writing in its worktree was getting
        // diagnostics (and baseline overwrites) from
        // the main repo — the model saw errors it did
        // not introduce.
        match kod_tools::CheckTool::run_check(&self.working_dir, 30).await {
            Ok(outcome) => {
                let n = outcome.diagnostics.len();
                *self.check_baseline.write().await = Some(outcome.diagnostics);
                tracing::debug!(count = n, "baseline captured");
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "baseline not captured (no project or toolchain)"
                );
            }
        }
    }
}
