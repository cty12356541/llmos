//! W39-D / Stage D: §25 Surface 窗口开合终态(REGISTERED…CLOSED)。
//!
//! 规范最小生命周期:
//! `Surface: REGISTERED → CREATED → PRESENTED ↔ HIDDEN → CLOSED`
//!
//! 本测试钉死 open 后 close 到达规范终态 `CLOSED`(不引入 WM/合成器)。

use llmos_desktop_lib::window_lifecycle::SurfaceLifecycle;

#[test]
fn open_then_close_reaches_closed() {
    let state = SurfaceLifecycle::registered()
        .open()
        .expect("open from REGISTERED")
        .close()
        .expect("close after open");
    assert_eq!(state, SurfaceLifecycle::Closed);
}
