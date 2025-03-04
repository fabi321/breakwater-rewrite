#[cfg(feature = "vnc")]
use breakwater::{
    args::Args,
    framebuffer::FrameBuffer,
    network::Network,
    prometheus_exporter::PrometheusExporter,
    sinks::{ffmpeg::FfmpegSink, vnc::VncServer},
    statistics::{Statistics, StatisticsEvent, StatisticsInformationEvent, StatisticsSaveMode},
};
use clap::Parser;
use env_logger::Env;
use std::sync::Arc;
#[cfg(feature = "vnc")]
use thread_priority::{ThreadBuilderExt, ThreadPriority};
use tokio::sync::{broadcast, mpsc};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let fb = Arc::new(FrameBuffer::new(args.width, args.height));

    // If we make the channel to big, stats will start to lag behind
    // TODO: Check performance impact in real-world scenario. Maybe the statistics thread blocks the other threads
    let (statistics_tx, statistics_rx) = mpsc::channel::<StatisticsEvent>(100);
    let (statistics_information_tx, statistics_information_rx_for_prometheus_exporter) =
        broadcast::channel::<StatisticsInformationEvent>(2);
    #[cfg(feature = "vnc")]
    let statistics_information_rx_for_vnc_server = statistics_information_tx.subscribe();

    let statistics_save_mode = if args.disable_statistics_save_file {
        StatisticsSaveMode::Disabled
    } else {
        StatisticsSaveMode::Enabled {
            save_file: args.statistics_save_file,
            interval_s: args.statistics_save_interval_s,
        }
    };
    let mut statistics = Statistics::new(
        statistics_rx,
        statistics_information_tx,
        statistics_save_mode,
    )?;

    let network = Network::new(args.listen_address, Arc::clone(&fb), statistics_tx.clone());
    let network_listener_thread = tokio::spawn(async move {
        network.listen().await.unwrap();
    });

    let ffmpeg_thread = if args.rtmp {
        let ffmpeg_sink = FfmpegSink::new(Arc::clone(&fb));
        Some(tokio::spawn(async move {
            ffmpeg_sink.run(&args.rtmp_address).await.unwrap();
        }))
    } else {
        None
    };

    #[cfg(feature = "vnc")]
    let vnc_server_thread = {
        let fb_for_vnc_server = Arc::clone(&fb);
        // TODO Use tokio::spawn instead of std::thread::spawn
        // I was not able to get to work with async closure
        // We than also need to think about setting a priority
        std::thread::Builder::new()
        .name("breakwater vnc server thread".to_owned())
        .spawn_with_priority(
            ThreadPriority::Crossplatform(70.try_into().expect("Failed to get cross-platform ThreadPriority. Please report this error message together with your operating system.")),
            move |_| {
                let mut vnc_server = VncServer::new(
                    fb_for_vnc_server,
                    args.vnc_port,
                    args.fps,
                    statistics_tx,
                    statistics_information_rx_for_vnc_server,
                    &args.text,
                    &args.font,
                );
                vnc_server.run();
            },
        )
        .unwrap()
    };

    let statistics_thread =
        tokio::spawn(async move { statistics.start().await.expect("Statistics thread failed") });

    let mut prometheus_exporter = PrometheusExporter::new(
        &args.prometheus_listen_address,
        statistics_information_rx_for_prometheus_exporter,
    );
    let prometheus_exporter_thread = tokio::spawn(async move {
        prometheus_exporter.run().await;
    });

    prometheus_exporter_thread.await?;
    network_listener_thread.await?;
    if let Some(ffmpeg_thread) = ffmpeg_thread {
        ffmpeg_thread.await?;
    }
    statistics_thread.await?;
    #[cfg(feature = "vnc")]
    {
        vnc_server_thread
            .join()
            .expect("Failed to join VNC server thread");
    }

    Ok(())
}
