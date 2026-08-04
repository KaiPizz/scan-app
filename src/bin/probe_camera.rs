//! Diagnostic: what the IRIScan actually delivers over USB and which image
//! controls (white balance, exposure, focus…) it exposes. Console binary.
//!
//! Set PROBE_OUT=<path.png> to also dump one raw full-resolution frame.
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{
    ApiBackend, CameraFormat, ControlValueSetter, FrameFormat, KnownCameraControl, RequestedFormat,
    RequestedFormatType,
};
use nokhwa::{Camera, query};

fn main() {
    // MediaFoundation work runs on a worker thread, like the app does.
    let worker = std::thread::spawn(probe);
    if worker.join().is_err() {
        println!("WORKER_PANICKED");
    }
}

fn probe() {
    let cameras = match query(ApiBackend::MediaFoundation) {
        Ok(cameras) => cameras,
        Err(error) => {
            println!("QUERY_FAILED: {error}");
            return;
        }
    };
    for info in &cameras {
        println!("DEVICE: {} | {:?}", info.human_name(), info.index());
    }
    let wanted = std::env::var("PROBE_DEVICE").unwrap_or_else(|_| "iriscan".to_owned());
    let Some(info) = cameras
        .iter()
        .find(|camera| camera.human_name().to_lowercase().contains(&wanted))
    else {
        println!("NO_DEVICE_MATCHING: {wanted}");
        return;
    };
    println!("USING: {}", info.human_name());

    if std::env::var("PROBE_MODE").as_deref() == Ok("enum") {
        let request = RequestedFormat::new::<RgbFormat>(RequestedFormatType::None);
        println!("ENUM: opening with RequestedFormatType::None…");
        let mut probe = match Camera::with_backend(
            info.index().clone(),
            request,
            ApiBackend::MediaFoundation,
        ) {
            Ok(camera) => camera,
            Err(error) => {
                println!("ENUM_OPEN_FAILED: {error}");
                return;
            }
        };
        println!("ENUM_OPENED: {:?}", probe.camera_format());
        match probe.compatible_camera_formats() {
            Ok(formats) => {
                println!("ENUM_TOTAL: {}", formats.len());
                for format in formats {
                    println!(
                        "ENUM_FORMAT: {}x{} {:?} {}fps",
                        format.resolution().width(),
                        format.resolution().height(),
                        format.format(),
                        format.frame_rate()
                    );
                }
            }
            Err(error) => println!("ENUM_LIST_FAILED: {error}"),
        }
        match probe.camera_controls() {
            Ok(controls) => {
                for control in controls {
                    println!("ENUM_CONTROL: {control:?}");
                }
            }
            Err(error) => println!("ENUM_CONTROLS_FAILED: {error}"),
        }
        println!("DONE");
        return;
    }

    // Open directly with exact formats.
    // Real device formats in the same order the app sorts them.
    let candidates = [
        CameraFormat::new_from(3840, 2880, FrameFormat::MJPEG, 15),
        CameraFormat::new_from(3840, 2160, FrameFormat::MJPEG, 30),
        CameraFormat::new_from(3264, 2448, FrameFormat::MJPEG, 30),
        CameraFormat::new_from(2592, 1944, FrameFormat::MJPEG, 30),
        CameraFormat::new_from(2048, 1536, FrameFormat::MJPEG, 30),
        CameraFormat::new_from(1920, 1080, FrameFormat::MJPEG, 30),
        CameraFormat::new_from(1600, 1200, FrameFormat::MJPEG, 30),
    ];
    for format in candidates {
        println!(
            "TRY: {}x{} {:?} {}fps",
            format.resolution().width(),
            format.resolution().height(),
            format.format(),
            format.frame_rate()
        );
        let request = RequestedFormat::new::<RgbFormat>(RequestedFormatType::Exact(format));
        let mut camera = match Camera::with_backend(
            info.index().clone(),
            request,
            ApiBackend::MediaFoundation,
        ) {
            Ok(camera) => camera,
            Err(error) => {
                println!("  OPEN_FAILED: {error}");
                continue;
            }
        };
        if let Err(error) = camera.open_stream() {
            println!("  STREAM_FAILED: {error}");
            continue;
        }
        println!("  NEGOTIATED: {:?}", camera.camera_format());
        match camera.camera_controls() {
            Ok(controls) => {
                for control in controls {
                    println!("  CONTROL: {control:?}");
                }
            }
            Err(error) => println!("  CONTROLS_FAILED: {error}"),
        }
        let mut last = None;
        let mut failures = 0;
        for index in 0..8 {
            match camera
                .frame()
                .and_then(|buffer| buffer.decode_image::<RgbFormat>())
            {
                Ok(frame) => {
                    if index == 0 {
                        println!("  FRAME OK: {}x{}", frame.width(), frame.height());
                    }
                    last = Some(frame);
                }
                Err(error) => {
                    failures += 1;
                    if index == 0 {
                        println!("  FRAME FAILED: {error}");
                    }
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        let _ = camera.stop_stream();
        if last.is_none() || failures > 4 {
            println!("  UNUSABLE (failures: {failures}) — next candidate");
            drop(camera);
            std::thread::sleep(std::time::Duration::from_millis(600));
            continue;
        }
        println!(
            "  >>> THIS IS WHAT THE APP GETS: {}x{} MJPEG",
            format.resolution().width(),
            format.resolution().height()
        );

        // White-balance sweep: put a blank white sheet under the camera first.
        if std::env::var("PROBE_MODE").as_deref() == Ok("wb") {
            if std::env::var("PROBE_NO_BLC").is_ok() {
                match camera.set_camera_control(
                    KnownCameraControl::BacklightComp,
                    ControlValueSetter::Boolean(false),
                ) {
                    Ok(()) => println!("  BACKLIGHT_COMP: off"),
                    Err(error) => println!("  BACKLIGHT_COMP_FAILED: {error}"),
                }
            }
            for value in (0..=44).step_by(2) {
                if let Err(error) = camera.set_camera_control(
                    KnownCameraControl::WhiteBalance,
                    ControlValueSetter::Integer(value),
                ) {
                    println!("  WB {value}: SET_FAILED {error}");
                    continue;
                }
                std::thread::sleep(std::time::Duration::from_millis(700));
                let mut measured = None;
                for _ in 0..3 {
                    if let Ok(frame) = camera
                        .frame()
                        .and_then(|buffer| buffer.decode_image::<RgbFormat>())
                    {
                        measured = Some(frame);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(120));
                }
                let Some(frame) = measured else {
                    println!("  WB {value}: NO_FRAME");
                    continue;
                };
                let (width, height) = (frame.width(), frame.height());
                let mut sums = [0_u64; 3];
                let mut count = 0_u64;
                for y in (height / 3..height * 2 / 3).step_by(8) {
                    for x in (width / 3..width * 2 / 3).step_by(8) {
                        let pixel = frame.get_pixel(x, y);
                        if pixel[1] > 120 {
                            for channel in 0..3 {
                                sums[channel] += pixel[channel] as u64;
                            }
                            count += 1;
                        }
                    }
                }
                if count == 0 {
                    println!("  WB {value}: brak jasnych pikseli (połóż białą kartkę)");
                    continue;
                }
                let means: Vec<f64> = (0..3)
                    .map(|channel| sums[channel] as f64 / count as f64)
                    .collect();
                let spread = means[2] - means[0];
                println!(
                    "  WB {value:>2}: R={:.0} G={:.0} B={:.0}  B-R={:+.0}",
                    means[0], means[1], means[2], spread
                );
            }
            println!("DONE");
            return;
        }
        match camera.camera_controls() {
            Ok(controls) => {
                for control in controls {
                    println!("  CONTROL: {control:?}");
                }
            }
            Err(error) => println!("  CONTROLS_FAILED: {error}"),
        }
        if let (Some(frame), Ok(path)) = (last, std::env::var("PROBE_OUT")) {
            match frame.save(&path) {
                Ok(()) => println!("  SAVED: {path}"),
                Err(error) => println!("  SAVE_FAILED: {error}"),
            }
        }
        println!("DONE");
        return;
    }
    println!("NO_FORMAT_WORKED");
}
