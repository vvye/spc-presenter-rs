use std::thread;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use crate::renderer::{Renderer, render_options::RendererOptions};

#[derive(Clone)]
pub enum RenderThreadRequest {
    StartRender(RendererOptions),
    CancelRender,
    Terminate
}

#[derive(Clone)]
pub struct RenderProgressInfo {
    pub frame: u64,
    pub average_fps: u32,
    pub encoded_size: usize,
    pub expected_duration_frames: Option<usize>,
    pub expected_duration: Option<Duration>,
    pub eta_duration: Option<Duration>,
    pub elapsed_duration: Duration,
    pub encoded_duration: Duration,
    pub loop_count: u64
}

#[derive(Clone)]
pub enum RenderThreadMessage {
    Error(String),
    RenderStarting,
    RenderProgress(RenderProgressInfo),
    RenderComplete,
    RenderCancelled
}

macro_rules! rt_unwrap {
    ($v: expr, $cb: tt, $lbl: tt) => {
        match $v {
            Ok(v) => v,
            Err(e) => {
                $cb(RenderThreadMessage::Error(e));
                continue $lbl;
            }
        }
    };
}

pub fn render_thread<F>(cb: F) -> (thread::JoinHandle<()>, mpsc::Sender<RenderThreadRequest>)
    where
        F: Fn(RenderThreadMessage) + Send + 'static
{
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        println!("Renderer thread started");

        'main: loop {
            let options = match rx.recv().unwrap() {
                RenderThreadRequest::StartRender(o) => o,
                RenderThreadRequest::CancelRender => {
                    cb(RenderThreadMessage::Error("No active render to cancel.".to_string()));
                    continue 'main;
                }
                RenderThreadRequest::Terminate => break 'main
            };
            cb(RenderThreadMessage::RenderStarting);

            let mut renderer = rt_unwrap!(Renderer::new(options), cb, 'main);
            rt_unwrap!(renderer.start_encoding(), cb, 'main);

            let mut last_progress_timestamp = Instant::now();
            // Janky way to force an update
            last_progress_timestamp.checked_sub(Duration::from_secs(2));

            'render: loop {
                match rx.try_recv() {
                    Ok(RenderThreadRequest::StartRender(_)) => {
                        cb(RenderThreadMessage::Error("No active render to cancel.".to_string()))
                    },
                    Ok(RenderThreadRequest::CancelRender) => {
                        cb(RenderThreadMessage::RenderCancelled);
                        break 'render;
                    },
                    Ok(RenderThreadRequest::Terminate) => break 'main,
                    _ => ()
                }
                if !(rt_unwrap!(renderer.step(), cb, 'main)) {
                    break;
                }

                if last_progress_timestamp.elapsed().as_secs_f64() >= 0.5 {
                    last_progress_timestamp = Instant::now();

                    let progress_info = RenderProgressInfo {
                        frame: renderer.current_frame(),
                        average_fps: renderer.average_fps(),
                        encoded_size: renderer.encoded_size(),
                        expected_duration_frames: renderer.expected_duration_frames(),
                        expected_duration: renderer.expected_duration(),
                        eta_duration: renderer.eta_duration(),
                        elapsed_duration: renderer.elapsed(),
                        encoded_duration: renderer.encoded_duration(),
                        loop_count: renderer.loop_count()
                    };

                    cb(RenderThreadMessage::RenderProgress(progress_info));
                }
            }

            rt_unwrap!(renderer.finish_encoding(), cb, 'main);
            cb(RenderThreadMessage::RenderComplete);
        }
    });
    (handle, tx)
}