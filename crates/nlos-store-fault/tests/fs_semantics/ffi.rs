//! Minimal safe wrapper over the raw `libsqlite3-sys` C API for the
//! filesystem-semantics probes.
//!
//! `nlos-store-fault` deliberately has no `rusqlite` dependency (only the
//! version-pinned `libsqlite3-sys`), so the test suite drives `SQLite` through
//! a tiny hand-rolled wrapper. Every `unsafe` block is confined here and
//! carries a `SAFETY` justification; the public surface of this module is
//! safe.

use std::ffi::{CStr, CString};
use std::path::Path;
use std::ptr;

use libsqlite3_sys as ffi;

/// A `SQLite` connection handle with RAII close semantics.
pub(crate) struct RawDb {
    db: *mut ffi::sqlite3,
}

impl RawDb {
    /// Opens (creating when missing) the database file at `path`, optionally
    /// through a named VFS.
    pub(crate) fn open(path: &Path, vfs: Option<&str>) -> RawDb {
        let c_path = CString::new(path.to_str().expect("utf8 database path"))
            .expect("interior NUL in database path");
        let c_vfs = vfs.map(|name| CString::new(name).expect("interior NUL in VFS name"));
        let mut db = ptr::null_mut();
        // SAFETY: `c_path`/`c_vfs` are live NUL-terminated strings for the
        // call duration; `db` is an out-parameter initialized by SQLite.
        let rc = unsafe {
            ffi::sqlite3_open_v2(
                c_path.as_ptr(),
                &raw mut db,
                ffi::SQLITE_OPEN_READWRITE | ffi::SQLITE_OPEN_CREATE | ffi::SQLITE_OPEN_FULLMUTEX,
                c_vfs.as_ref().map_or(ptr::null(), |v| v.as_ptr()),
            )
        };
        assert_eq!(rc, ffi::SQLITE_OK, "sqlite3_open_v2 failed with rc={rc}");
        RawDb { db }
    }

    /// Runs one SQL batch, failing the test on any error.
    pub(crate) fn exec(&self, sql: &str) {
        self.exec_result(sql).expect("exec failed");
    }

    /// Runs one SQL batch and returns the raw `SQLite` error code.
    pub(crate) fn exec_result(&self, sql: &str) -> Result<(), i32> {
        let c_sql = CString::new(sql).expect("interior NUL in SQL");
        let mut errmsg: *mut std::ffi::c_char = ptr::null_mut();
        // SAFETY: `c_sql` is a live NUL-terminated string; the callback and
        // tail pointers are unused; `errmsg` is an out-parameter freed below.
        let rc = unsafe {
            ffi::sqlite3_exec(
                self.db,
                c_sql.as_ptr(),
                None,
                ptr::null_mut(),
                &raw mut errmsg,
            )
        };
        if rc != ffi::SQLITE_OK {
            let message = if errmsg.is_null() {
                format!("code {rc}")
            } else {
                // SAFETY: on failure `SQLite` stores a NUL-terminated message
                // in `errmsg` that stays valid until `sqlite3_free`.
                let message = unsafe { CStr::from_ptr(errmsg.cast()) }
                    .to_string_lossy()
                    .into_owned();
                // SAFETY: `errmsg` was allocated by SQLite and must be
                // released with `sqlite3_free` exactly once.
                unsafe { ffi::sqlite3_free(errmsg.cast()) };
                message
            };
            eprintln!("exec({sql:?}) failed: rc={rc}: {message}");
            return Err(rc);
        }
        Ok(())
    }

    /// Reads the first column of every row as text, failing the test on
    /// error.
    pub(crate) fn query_texts(&self, sql: &str) -> Vec<String> {
        self.query_texts_result(sql).expect("query failed")
    }

    /// Reads the first column of every row as text, returning the raw `SQLite`
    /// error code on failure.
    pub(crate) fn query_texts_result(&self, sql: &str) -> Result<Vec<String>, i32> {
        let mut texts = Vec::new();
        self.collect_first_column(sql, &mut |stmt| {
            // SAFETY: the statement is positioned on a row and column 0
            // exists; the returned buffer is valid until the next step on
            // this statement.
            let raw = unsafe { ffi::sqlite3_column_text(stmt, 0) };
            texts.push(if raw.is_null() {
                String::new()
            } else {
                // SAFETY: `sqlite3_column_text` yields a NUL-terminated UTF-8
                // buffer valid until the next step/finalize on this stmt.
                unsafe { CStr::from_ptr(raw.cast()) }
                    .to_string_lossy()
                    .into_owned()
            });
        })
        .inspect_err(|code| eprintln!("query({sql:?}) failed: rc={code}"))?;
        Ok(texts)
    }

    /// Reads the first column of every row as an integer, failing the test
    /// on error.
    pub(crate) fn query_ints(&self, sql: &str) -> Vec<i64> {
        self.query_ints_result(sql).expect("query failed")
    }

    /// Reads the first column of every row as an integer, returning the raw
    /// `SQLite` error code on failure.
    pub(crate) fn query_ints_result(&self, sql: &str) -> Result<Vec<i64>, i32> {
        let mut ints = Vec::new();
        self.collect_first_column(sql, &mut |stmt| {
            // SAFETY: the statement is positioned on a row and column 0
            // exists for every query used by this suite.
            ints.push(unsafe { ffi::sqlite3_column_int64(stmt, 0) });
        })
        .inspect_err(|code| eprintln!("query({sql:?}) failed: rc={code}"))?;
        Ok(ints)
    }

    /// Shared stepper: runs `sql` and invokes `sink` once per row with the
    /// live statement handle. Returns `Err(rc)` for any non-ROW/DONE step
    /// result.
    fn collect_first_column(
        &self,
        sql: &str,
        sink: &mut dyn FnMut(*mut ffi::sqlite3_stmt),
    ) -> Result<(), i32> {
        let c_sql = CString::new(sql).expect("interior NUL in SQL");
        let mut stmt = ptr::null_mut();
        // SAFETY: `c_sql` is a live NUL-terminated string; `stmt` is an
        // out-parameter finalized below on every path.
        let rc = unsafe {
            ffi::sqlite3_prepare_v2(self.db, c_sql.as_ptr(), -1, &raw mut stmt, ptr::null_mut())
        };
        if rc != ffi::SQLITE_OK {
            return Err(rc);
        }
        // SAFETY: `stmt` is initialized (prepare succeeded above).
        let guard = StmtGuard(stmt);
        loop {
            // SAFETY: `guard.0` is a live statement handle.
            let rc = unsafe { ffi::sqlite3_step(guard.0) };
            match rc {
                ffi::SQLITE_ROW => sink(guard.0),
                ffi::SQLITE_DONE => return Ok(()),
                _ => return Err(rc),
            }
        }
    }

    /// `PRAGMA integrity_check` first row; must be `"ok"` for a healthy
    /// database.
    pub(crate) fn assert_integrity(&self) {
        let verdicts = self.query_texts("PRAGMA integrity_check");
        assert_eq!(verdicts, ["ok"], "integrity_check must pass");
    }

    /// Switches the connection to WAL and to `synchronous`, then reads both
    /// pragmas back so a silent fallback fails loudly.
    pub(crate) fn configure_wal(&self, synchronous: &str) {
        self.exec("PRAGMA journal_mode=WAL");
        self.exec(&format!("PRAGMA synchronous={synchronous}"));
        let journal = self.query_texts("PRAGMA journal_mode");
        assert_eq!(
            journal,
            ["wal"],
            "journal_mode must be WAL, got {journal:?}"
        );
        let sync = self.query_texts("PRAGMA synchronous");
        let expected = match synchronous {
            "FULL" => "2",
            "NORMAL" => "1",
            other => panic!("unsupported synchronous mode {other}"),
        };
        assert_eq!(
            sync,
            [expected],
            "synchronous must be {expected}, got {sync:?}"
        );
    }
}

impl Drop for RawDb {
    fn drop(&mut self) {
        if !self.db.is_null() {
            // SAFETY: `self.db` is the connection handle opened in
            // `RawDb::open`; every statement is finalized before drop (see
            // `StmtGuard`), so `sqlite3_close` cannot report SQLITE_BUSY.
            // This SQLite build does not expose `sqlite3_close_v2`.
            unsafe { ffi::sqlite3_close(self.db) };
            self.db = ptr::null_mut();
        }
    }
}

/// Finalizes a prepared statement on drop.
struct StmtGuard(*mut ffi::sqlite3_stmt);

impl Drop for StmtGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `self.0` is a live statement handle from
            // `sqlite3_prepare_v2`, finalized exactly once here.
            unsafe { ffi::sqlite3_finalize(self.0) };
        }
    }
}
