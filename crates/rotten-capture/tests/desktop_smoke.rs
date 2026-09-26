//! Explicit local check; excluded from headless CI and ordinary test runs.

#[test]
#[ignore = "requires an unlocked desktop; captures pixels in memory without saving them"]
fn desktop_capture_returns_complete_rgba_frames() {
    let displays = rotten_capture::list_displays().expect("enumerate desktop displays");
    let display = displays.first().expect("an active desktop display");
    let mut capture = rotten_capture::create_capture_backend(Some(display.index), false)
        .expect("create desktop capture backend");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let mut received = 0;
    let mut last_error = None;
    while received < 3 && std::time::Instant::now() < deadline {
        match capture.grab_frame() {
            Ok(frame) => {
                assert!(frame.width > 0 && frame.height > 0);
                assert_eq!(
                    frame.rgba.len(),
                    frame.width as usize * frame.height as usize * 4
                );
                received += 1;
            }
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert_eq!(received, 3, "capture failed: {last_error:?}");
}

/// The forced GDI fallback must work on its own, independent of the auto path.
#[cfg(target_os = "windows")]
#[test]
#[ignore = "requires an unlocked desktop; captures pixels in memory without saving them"]
fn gdi_fallback_capture_returns_complete_rgba_frames() {
    let mut capture = rotten_capture::gdi::create_windows_gdi_backend(Some(0), false)
        .expect("create GDI capture backend");
    assert_eq!(capture.backend_name(), "gdi");
    for _ in 0..3 {
        let frame = capture.grab_frame().expect("GDI frame");
        assert!(frame.width > 0 && frame.height > 0);
        assert_eq!(
            frame.rgba.len(),
            frame.width as usize * frame.height as usize * 4
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
