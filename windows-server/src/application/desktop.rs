use std::{sync::mpsc, time::Duration};

use anyhow::Context;
use clap::Parser;
use tokio_util::sync::CancellationToken;

use crate::{audio, config::ServerArgs, ui};

pub fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();

    let args = ServerArgs::parse();
    let runtime = tokio::runtime::Runtime::new().context("无法启动服务运行时")?;
    if let Some(path) = args.capture_to.as_deref() {
        anyhow::ensure!(args.capture_seconds > 0, "--capture-seconds 必须大于零");
        let duration = Duration::from_secs(args.capture_seconds);
        audio::capture::record_to_pcm(path, duration)?;
        ui::show_capture_complete(path);
        return Ok(());
    }

    let shutdown = CancellationToken::new();
    let (ready, receiver) = mpsc::sync_channel(1);
    let server_shutdown = shutdown.clone();
    let server = runtime.spawn(async move {
        let result = super::streamer::run(args, server_shutdown.clone(), ready.clone()).await;
        server_shutdown.cancel();
        if let Err(error) = &result {
            let _ = ready.try_send(Err(format!("{error:#}")));
        }
        result
    });
    let initialized = receiver.recv().context("服务在初始化完成前已停止");
    let result = match initialized {
        Ok(Ok(model)) => ui::run(model),
        Ok(Err(error)) => Err(anyhow::anyhow!(error)),
        Err(error) => Err(error),
    };
    shutdown.cancel();
    let server_result = runtime.block_on(server).context("服务任务异常终止")?;
    result.and(server_result)
}
