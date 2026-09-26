//! W39-D / Stage D: §25 Surface 窗口开合终态(REGISTERED…CLOSED)。
//!
//! 规范最小生命周期:
//! `Surface: REGISTERED → CREATED → PRESENTED ↔ HIDDEN → CLOSED`
//!
//! 本测试钉死 open 后 close 到达规范终态 `CLOSED`，以及
//! `REGISTERED → CREATED` 与 `PRESENTED ↔ HIDDEN`(不引入 WM/合成器)。

use llmos_desktop_lib::window_lifecycle::{SurfaceLifecycle, SurfaceLifecycleError};

#[test]
fn open_then_close_reaches_closed() {
    let state = SurfaceLifecycle::registered()
        .open()
        .expect("open from REGISTERED")
        .close()
        .expect("close after open");
    assert_eq!(state, SurfaceLifecycle::Closed);
}

#[test]
fn create_open_hide_reopen_follows_the_named_cycle() {
    let created = SurfaceLifecycle::registered()
        .create()
        .expect("REGISTERED → CREATED");
    assert_eq!(created, SurfaceLifecycle::Created);
    assert_eq!(
        created.create().expect("idempotent create"),
        SurfaceLifecycle::Created
    );

    let presented = created.open().expect("CREATED → PRESENTED");
    assert_eq!(presented, SurfaceLifecycle::Presented);
    let hidden = presented.hide().expect("PRESENTED → HIDDEN");
    assert_eq!(hidden, SurfaceLifecycle::Hidden);
    assert_eq!(
        hidden.hide().expect("idempotent hide"),
        SurfaceLifecycle::Hidden
    );
    assert_eq!(
        hidden.open().expect("HIDDEN → PRESENTED"),
        SurfaceLifecycle::Presented
    );
}

#[test]
fn create_and_hide_reject_states_outside_their_edges() {
    assert_eq!(
        SurfaceLifecycle::registered().hide(),
        Err(SurfaceLifecycleError::NotPresented)
    );
    assert_eq!(
        SurfaceLifecycle::registered()
            .create()
            .expect("create")
            .hide(),
        Err(SurfaceLifecycleError::NotPresented)
    );
    let presented = SurfaceLifecycle::registered().open().expect("open");
    assert_eq!(
        presented.create(),
        Err(SurfaceLifecycleError::NotRegistered)
    );
    let closed = presented.close().expect("close");
    assert_eq!(closed.hide(), Err(SurfaceLifecycleError::AlreadyClosed));
    assert_eq!(closed.create(), Err(SurfaceLifecycleError::AlreadyClosed));
}
