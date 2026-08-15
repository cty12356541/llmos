//! Durable local Driver/Device/Reservation binding authority.
//!
//! This Stage-B reference slice proves pre-dispatch reserve/activate fencing.
//! It is not the complete multidimensional Resource Manager or final ledger.

mod schema;

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_types::{
    CallId, DeviceId, DriverId, Generation, IdempotencyKey, OperationId, QuoteId, ReceiptId,
    ReservationId, ResourceAccountId, TaskParticipantId,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

pub type FencingToken = [u8; 32];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterDriverRequest {
    pub profile_digest: [u8; 32],
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverRecord {
    pub driver_id: DriverId,
    pub device_id: DeviceId,
    pub generation: Generation,
    pub fencing_token: FencingToken,
    pub profile_digest: [u8; 32],
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverGatewayEndpointProof {
    pub driver_id: DriverId,
    pub participant_id: TaskParticipantId,
    pub participant_generation: Generation,
    pub admission_receipt_id: ReceiptId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverDecision {
    Registered(DriverRecord),
    Replayed(DriverRecord),
}
impl DriverDecision {
    #[must_use]
    pub const fn record(self) -> DriverRecord {
        match self {
            Self::Registered(r) | Self::Replayed(r) => r,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RotateDriverRequest {
    pub driver_id: DriverId,
    pub expected_generation: Generation,
    pub expected_fencing_token: FencingToken,
    pub idempotency_key: IdempotencyKey,
    pub rotated_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverRotationDecision {
    Rotated(DriverRecord),
    Replayed(DriverRecord),
}
impl DriverRotationDecision {
    #[must_use]
    pub const fn record(self) -> DriverRecord {
        match self {
            Self::Rotated(r) | Self::Replayed(r) => r,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateAccountRequest {
    pub initial_credit: u64,
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountRecord {
    pub account_id: ResourceAccountId,
    pub initial_credit: u64,
    pub available_credit: u64,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceLedgerEndpointProof {
    pub account_id: ResourceAccountId,
    pub participant_id: TaskParticipantId,
    pub participant_generation: Generation,
    pub admission_receipt_id: ReceiptId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateQuoteRequest {
    pub driver_id: DriverId,
    pub driver_generation: Generation,
    pub driver_fencing_token: FencingToken,
    pub operation_proposal_digest: [u8; 32],
    pub pricing_version: [u8; 32],
    pub upper_bound: u64,
    pub valid_until_ms: u64,
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuoteRecord {
    pub quote_id: QuoteId,
    pub driver_id: DriverId,
    pub device_id: DeviceId,
    pub driver_generation: Generation,
    pub driver_fencing_token: FencingToken,
    pub operation_proposal_digest: [u8; 32],
    pub pricing_version: [u8; 32],
    pub upper_bound: u64,
    pub valid_until_ms: u64,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuoteDecision {
    Created(QuoteRecord),
    Replayed(QuoteRecord),
}
impl QuoteDecision {
    #[must_use]
    pub const fn record(self) -> QuoteRecord {
        match self {
            Self::Created(r) | Self::Replayed(r) => r,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReserveRequest {
    pub account_id: ResourceAccountId,
    pub quote_id: QuoteId,
    pub call_id: CallId,
    pub operation_id: OperationId,
    pub idempotency_key: IdempotencyKey,
    pub reserved_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationState {
    Reserved,
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReservationRecord {
    pub reservation_id: ReservationId,
    pub account_id: ResourceAccountId,
    pub quote_id: QuoteId,
    pub call_id: CallId,
    pub operation_id: OperationId,
    pub driver_id: DriverId,
    pub device_id: DeviceId,
    pub driver_generation: Generation,
    pub driver_fencing_token: FencingToken,
    pub upper_bound: u64,
    pub activation_token: [u8; 32],
    pub state: ReservationState,
    pub created_at_ms: u64,
    pub activation_receipt_id: Option<ReceiptId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReservationDecision {
    Reserved(ReservationRecord),
    Replayed(ReservationRecord),
}
impl ReservationDecision {
    #[must_use]
    pub const fn record(self) -> ReservationRecord {
        match self {
            Self::Reserved(r) | Self::Replayed(r) => r,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivateReservationRequest {
    pub reservation_id: ReservationId,
    pub call_id: CallId,
    pub operation_id: OperationId,
    pub driver_id: DriverId,
    pub driver_generation: Generation,
    pub driver_fencing_token: FencingToken,
    pub activation_token: [u8; 32],
    pub activated_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActivationReceipt {
    pub receipt_id: ReceiptId,
    pub reservation_id: ReservationId,
    pub operation_id: OperationId,
    pub activated_at_ms: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActivationDecision {
    Activated(ActivationReceipt),
    Replayed(ActivationReceipt),
}
impl ActivationDecision {
    #[must_use]
    pub const fn receipt(self) -> ActivationReceipt {
        match self {
            Self::Activated(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

#[derive(Debug)]
pub enum ResourceAuthorityError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    IdempotencyConflict,
    DriverNotFound,
    AccountNotFound,
    QuoteNotFound,
    ReservationNotFound,
    StaleDriver,
    InvalidUpperBound,
    QuoteExpired,
    InsufficientCredit {
        available: u64,
        required: u64,
    },
    ReservationBindingMismatch,
    ReservationAlreadyActive,
    ReservationNotActive,
    CorruptRecord(&'static str),
    GenerationExhausted,
    LockPoisoned,
}
impl fmt::Display for ResourceAuthorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl Error for ResourceAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        if let Self::Sqlite(e) = self {
            Some(e)
        } else if let Self::Io(e) = self {
            Some(e)
        } else {
            None
        }
    }
}
impl From<rusqlite::Error> for ResourceAuthorityError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

pub struct ResourceAuthority {
    connection: Mutex<Connection>,
}
#[allow(clippy::many_single_char_names)]
impl ResourceAuthority {
    /// Opens the WAL/FULL local reference authority.
    ///
    /// # Errors
    /// Fails when storage, durability, or schema validation fails.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ResourceAuthorityError> {
        std::fs::create_dir_all(root.as_ref()).map_err(ResourceAuthorityError::Io)?;
        let mut c = Connection::open(root.as_ref().join("resource-authority.db"))?;
        c.busy_timeout(Duration::from_secs(5))?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "synchronous", "FULL")?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        let j: String = c.pragma_query_value(None, "journal_mode", |r| r.get(0))?;
        let s: i64 = c.pragma_query_value(None, "synchronous", |r| r.get(0))?;
        if !j.eq_ignore_ascii_case("wal") || s != 2 {
            return Err(ResourceAuthorityError::DurabilityUnavailable {
                journal_mode: j,
                synchronous: s,
            });
        }
        let v: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match v {
            0 => {
                schema::migrate_v1(&mut c)?;
                schema::migrate_v2(&mut c)?;
            }
            1 => schema::migrate_v2(&mut c)?,
            2 => {}
            x => return Err(ResourceAuthorityError::SchemaVersionUnsupported(x)),
        }
        Ok(Self {
            connection: Mutex::new(c),
        })
    }

    /// Registers an authority-assigned Driver/Device binding.
    ///
    /// # Errors
    /// Fails on key rebinding or storage failure.
    pub fn register_driver(
        &self,
        q: RegisterDriverRequest,
    ) -> Result<DriverDecision, ResourceAuthorityError> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(r) = driver_by_key(&tx, q.idempotency_key)? {
            if r.profile_digest != q.profile_digest {
                return Err(ResourceAuthorityError::IdempotencyConflict);
            }
            tx.commit()?;
            return Ok(DriverDecision::Replayed(r));
        }
        let did = DriverId::from_bytes(id16(
            b"nlos/driver/id/v1",
            &[q.idempotency_key.as_bytes(), &q.profile_digest],
        ));
        let dev = DeviceId::from_bytes(id16(
            b"nlos/device/id/v1",
            &[q.idempotency_key.as_bytes(), did.as_bytes()],
        ));
        let g = Generation::INITIAL;
        let token = hash(
            b"nlos/driver/fence/v1",
            &[
                did.as_bytes(),
                &g.get().to_be_bytes(),
                q.idempotency_key.as_bytes(),
            ],
        );
        let r = DriverRecord {
            driver_id: did,
            device_id: dev,
            generation: g,
            fencing_token: token,
            profile_digest: q.profile_digest,
            created_at_ms: q.created_at_ms,
        };
        tx.execute(
            "INSERT INTO drivers VALUES(?1,?2,?3,?4,?5,?6,?7,?7)",
            params![
                did.as_bytes().as_slice(),
                dev.as_bytes().as_slice(),
                q.idempotency_key.as_bytes().as_slice(),
                eg(g)?,
                token.as_slice(),
                q.profile_digest.as_slice(),
                eu(q.created_at_ms)?
            ],
        )?;
        insert_driver_generation(&tx, r)?;
        insert_initial_driver_gateway_endpoint(&tx, r)?;
        tx.commit()?;
        Ok(DriverDecision::Registered(r))
    }

    /// Rotates a Driver generation and fence by CAS.
    ///
    /// # Errors
    /// Fails on stale input, replay conflict, exhaustion, or storage failure.
    pub fn rotate_driver(
        &self,
        q: RotateDriverRequest,
    ) -> Result<DriverRotationDecision, ResourceAuthorityError> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((d,eg0,et,rg))=tx.query_row("SELECT driver_id,expected_generation,expected_fencing_token,resulting_generation FROM driver_rotations WHERE idempotency_key=?1",[q.idempotency_key.as_bytes().as_slice()],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,i64>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,i64>(3)?))).optional()? { if DriverId::from_bytes(a16(d)?)!=q.driver_id||dg(eg0)?!=q.expected_generation||a32(et)?!=q.expected_fencing_token{return Err(ResourceAuthorityError::IdempotencyConflict)} let r=driver_generation(&tx,q.driver_id,dg(rg)?)?;tx.commit()?;return Ok(DriverRotationDecision::Replayed(r)); }
        let cur = driver(&tx, q.driver_id)?.ok_or(ResourceAuthorityError::DriverNotFound)?;
        if cur.generation != q.expected_generation || cur.fencing_token != q.expected_fencing_token
        {
            return Err(ResourceAuthorityError::StaleDriver);
        }
        let g = cur
            .generation
            .checked_next()
            .ok_or(ResourceAuthorityError::GenerationExhausted)?;
        let token = hash(
            b"nlos/driver/fence/v1",
            &[
                q.driver_id.as_bytes(),
                &g.get().to_be_bytes(),
                q.idempotency_key.as_bytes(),
            ],
        );
        let r = DriverRecord {
            generation: g,
            fencing_token: token,
            created_at_ms: q.rotated_at_ms,
            ..cur
        };
        insert_driver_generation(&tx, r)?;
        insert_driver_gateway_generation_proof(&tx, r)?;
        tx.execute(
            "INSERT INTO driver_rotations VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                q.idempotency_key.as_bytes().as_slice(),
                q.driver_id.as_bytes().as_slice(),
                eg(q.expected_generation)?,
                q.expected_fencing_token.as_slice(),
                eg(g)?,
                token.as_slice(),
                eu(q.rotated_at_ms)?
            ],
        )?;
        let n=tx.execute("UPDATE drivers SET current_generation=?1,current_fencing_token=?2,updated_at_ms=?3 WHERE driver_id=?4 AND current_generation=?5 AND current_fencing_token=?6",params![eg(g)?,token.as_slice(),eu(q.rotated_at_ms)?,q.driver_id.as_bytes().as_slice(),eg(q.expected_generation)?,q.expected_fencing_token.as_slice()])?;
        if n != 1 {
            return Err(ResourceAuthorityError::StaleDriver);
        }
        tx.commit()?;
        Ok(DriverRotationDecision::Rotated(r))
    }

    /// Creates a bootstrap single-unit account for the reference profile.
    ///
    /// # Errors
    /// Fails on key rebinding, integer overflow, or storage failure.
    pub fn create_account(
        &self,
        q: CreateAccountRequest,
    ) -> Result<AccountRecord, ResourceAuthorityError> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(r) = account_by_key(&tx, q.idempotency_key)? {
            if r.initial_credit != q.initial_credit {
                return Err(ResourceAuthorityError::IdempotencyConflict);
            }
            tx.commit()?;
            return Ok(r);
        }
        let id = ResourceAccountId::from_bytes(id16(
            b"nlos/resource-account/id/v1",
            &[q.idempotency_key.as_bytes()],
        ));
        tx.execute(
            "INSERT INTO resource_accounts VALUES(?1,?2,?3,?3,?4)",
            params![
                id.as_bytes().as_slice(),
                q.idempotency_key.as_bytes().as_slice(),
                eu(q.initial_credit)?,
                eu(q.created_at_ms)?
            ],
        )?;
        insert_resource_ledger_endpoint(&tx, id)?;
        tx.commit()?;
        Ok(AccountRecord {
            account_id: id,
            initial_credit: q.initial_credit,
            available_credit: q.initial_credit,
            created_at_ms: q.created_at_ms,
        })
    }

    /// Creates an immutable quote bound to the current Driver fence.
    ///
    /// # Errors
    /// Fails on stale Driver input, invalid bounds, conflicts, or storage failure.
    pub fn create_quote(
        &self,
        q: CreateQuoteRequest,
    ) -> Result<QuoteDecision, ResourceAuthorityError> {
        if q.upper_bound == 0 {
            return Err(ResourceAuthorityError::InvalidUpperBound);
        }
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(r) = quote_by_key(&tx, q.idempotency_key)? {
            if !quote_matches(r, q) {
                return Err(ResourceAuthorityError::IdempotencyConflict);
            }
            tx.commit()?;
            return Ok(QuoteDecision::Replayed(r));
        }
        let d = active_driver(
            &tx,
            q.driver_id,
            q.driver_generation,
            q.driver_fencing_token,
        )?;
        let id = QuoteId::from_bytes(id16(
            b"nlos/quote/id/v1",
            &[
                q.idempotency_key.as_bytes(),
                q.operation_proposal_digest.as_slice(),
            ],
        ));
        let r = QuoteRecord {
            quote_id: id,
            driver_id: q.driver_id,
            device_id: d.device_id,
            driver_generation: q.driver_generation,
            driver_fencing_token: q.driver_fencing_token,
            operation_proposal_digest: q.operation_proposal_digest,
            pricing_version: q.pricing_version,
            upper_bound: q.upper_bound,
            valid_until_ms: q.valid_until_ms,
            created_at_ms: q.created_at_ms,
        };
        tx.execute(
            "INSERT INTO quotes VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                id.as_bytes().as_slice(),
                q.idempotency_key.as_bytes().as_slice(),
                r.driver_id.as_bytes().as_slice(),
                r.device_id.as_bytes().as_slice(),
                eg(r.driver_generation)?,
                r.driver_fencing_token.as_slice(),
                r.operation_proposal_digest.as_slice(),
                r.pricing_version.as_slice(),
                eu(r.upper_bound)?,
                eu(r.valid_until_ms)?,
                eu(r.created_at_ms)?
            ],
        )?;
        tx.commit()?;
        Ok(QuoteDecision::Created(r))
    }

    /// Atomically moves account credit into one immutable Operation reservation.
    ///
    /// # Errors
    /// Fails on stale/expired quote, insufficient credit, rebinding, or storage failure.
    pub fn reserve(
        &self,
        q: ReserveRequest,
    ) -> Result<ReservationDecision, ResourceAuthorityError> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(r) = reservation_by_key(&tx, q.idempotency_key)? {
            if r.account_id != q.account_id
                || r.quote_id != q.quote_id
                || r.call_id != q.call_id
                || r.operation_id != q.operation_id
            {
                return Err(ResourceAuthorityError::IdempotencyConflict);
            }
            tx.commit()?;
            return Ok(ReservationDecision::Replayed(r));
        }
        let qt = quote(&tx, q.quote_id)?.ok_or(ResourceAuthorityError::QuoteNotFound)?;
        active_driver(
            &tx,
            qt.driver_id,
            qt.driver_generation,
            qt.driver_fencing_token,
        )?;
        if q.reserved_at_ms > qt.valid_until_ms {
            return Err(ResourceAuthorityError::QuoteExpired);
        }
        let a = account(&tx, q.account_id)?.ok_or(ResourceAuthorityError::AccountNotFound)?;
        if a.available_credit < qt.upper_bound {
            return Err(ResourceAuthorityError::InsufficientCredit {
                available: a.available_credit,
                required: qt.upper_bound,
            });
        }
        let id = ReservationId::from_bytes(id16(
            b"nlos/reservation/id/v1",
            &[q.idempotency_key.as_bytes(), q.operation_id.as_bytes()],
        ));
        let token = hash(
            b"nlos/reservation/activate/v1",
            &[
                id.as_bytes(),
                q.operation_id.as_bytes(),
                q.idempotency_key.as_bytes(),
            ],
        );
        let n=tx.execute("UPDATE resource_accounts SET available_credit=available_credit-?1 WHERE account_id=?2 AND available_credit>=?1",params![eu(qt.upper_bound)?,q.account_id.as_bytes().as_slice()])?;
        if n != 1 {
            return Err(ResourceAuthorityError::InsufficientCredit {
                available: a.available_credit,
                required: qt.upper_bound,
            });
        }
        tx.execute("INSERT INTO reservations VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,0,?13,NULL,NULL)",params![id.as_bytes().as_slice(),q.idempotency_key.as_bytes().as_slice(),q.account_id.as_bytes().as_slice(),q.quote_id.as_bytes().as_slice(),q.call_id.as_bytes().as_slice(),q.operation_id.as_bytes().as_slice(),qt.driver_id.as_bytes().as_slice(),qt.device_id.as_bytes().as_slice(),eg(qt.driver_generation)?,qt.driver_fencing_token.as_slice(),eu(qt.upper_bound)?,token.as_slice(),eu(q.reserved_at_ms)?])?;
        let r = reservation(&tx, id)?.ok_or(ResourceAuthorityError::CorruptRecord(
            "new reservation absent",
        ))?;
        tx.commit()?;
        Ok(ReservationDecision::Reserved(r))
    }

    /// Returns a RESERVED binding only while its Driver fence is current.
    ///
    /// # Errors
    /// Fails for unknown, active, or stale-Driver reservations.
    pub fn inspect_permit_binding(
        &self,
        id: ReservationId,
    ) -> Result<ReservationRecord, ResourceAuthorityError> {
        let c = self.lock()?;
        let r = reservation(&c, id)?.ok_or(ResourceAuthorityError::ReservationNotFound)?;
        if r.state != ReservationState::Reserved {
            return Err(ResourceAuthorityError::ReservationAlreadyActive);
        }
        active_driver(&c, r.driver_id, r.driver_generation, r.driver_fencing_token)?;
        Ok(r)
    }

    /// Consumes the activation token exactly once before dispatch.
    ///
    /// # Errors
    /// Fails on any binding/fence mismatch or storage failure.
    pub fn activate(
        &self,
        q: ActivateReservationRequest,
    ) -> Result<ActivationDecision, ResourceAuthorityError> {
        let mut c = self.lock()?;
        let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let r = reservation(&tx, q.reservation_id)?
            .ok_or(ResourceAuthorityError::ReservationNotFound)?;
        if r.call_id != q.call_id
            || r.operation_id != q.operation_id
            || r.driver_id != q.driver_id
            || r.driver_generation != q.driver_generation
            || r.driver_fencing_token != q.driver_fencing_token
            || r.activation_token != q.activation_token
        {
            return Err(ResourceAuthorityError::ReservationBindingMismatch);
        }
        active_driver(
            &tx,
            r.driver_id,
            r.driver_generation,
            r.driver_fencing_token,
        )?;
        if r.state == ReservationState::Active {
            let x = activation_receipt(&tx, r.reservation_id)?.ok_or(
                ResourceAuthorityError::CorruptRecord("active reservation has no receipt"),
            )?;
            tx.commit()?;
            return Ok(ActivationDecision::Replayed(x));
        }
        let rid = ReceiptId::from_bytes(id16(
            b"nlos/reservation-activation/receipt/v1",
            &[r.reservation_id.as_bytes(), r.activation_token.as_slice()],
        ));
        tx.execute(
            "INSERT INTO reservation_activation_receipts VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                rid.as_bytes().as_slice(),
                r.reservation_id.as_bytes().as_slice(),
                r.call_id.as_bytes().as_slice(),
                r.operation_id.as_bytes().as_slice(),
                r.driver_id.as_bytes().as_slice(),
                eg(r.driver_generation)?,
                r.activation_token.as_slice(),
                eu(q.activated_at_ms)?
            ],
        )?;
        tx.execute("UPDATE reservations SET state=1,activated_at_ms=?1,activation_receipt_id=?2 WHERE reservation_id=?3 AND state=0",params![eu(q.activated_at_ms)?,rid.as_bytes().as_slice(),r.reservation_id.as_bytes().as_slice()])?;
        tx.commit()?;
        Ok(ActivationDecision::Activated(ActivationReceipt {
            receipt_id: rid,
            reservation_id: r.reservation_id,
            operation_id: r.operation_id,
            activated_at_ms: q.activated_at_ms,
        }))
    }

    /// Reads the immutable activation receipt of an ACTIVE Reservation.
    ///
    /// # Errors
    /// Fails when the Reservation is unknown, not active, missing its receipt,
    /// or the receipt no longer agrees with the durable Reservation row.
    pub fn inspect_activation_receipt(
        &self,
        reservation_id: ReservationId,
    ) -> Result<ActivationReceipt, ResourceAuthorityError> {
        let connection = self.lock()?;
        let reservation = reservation(&connection, reservation_id)?
            .ok_or(ResourceAuthorityError::ReservationNotFound)?;
        if reservation.state != ReservationState::Active {
            return Err(ResourceAuthorityError::ReservationNotActive);
        }
        let receipt = activation_receipt(&connection, reservation_id)?.ok_or(
            ResourceAuthorityError::CorruptRecord("active reservation has no receipt"),
        )?;
        if reservation.activation_receipt_id != Some(receipt.receipt_id)
            || reservation.operation_id != receipt.operation_id
        {
            return Err(ResourceAuthorityError::CorruptRecord(
                "activation receipt disagrees with reservation",
            ));
        }
        Ok(receipt)
    }

    /// Reads the current account balance.
    ///
    /// # Errors
    /// Fails if the account is absent or corrupt.
    pub fn inspect_account(
        &self,
        id: ResourceAccountId,
    ) -> Result<AccountRecord, ResourceAuthorityError> {
        let connection = self.lock()?;
        account(&connection, id)?.ok_or(ResourceAuthorityError::AccountNotFound)
    }

    /// Reads the endpoint proof for the current Driver generation.
    ///
    /// # Errors
    /// Fails for an unknown Driver, missing/corrupt proof, or storage error.
    pub fn inspect_driver_gateway_endpoint_proof(
        &self,
        id: DriverId,
    ) -> Result<DriverGatewayEndpointProof, ResourceAuthorityError> {
        let connection = self.lock()?;
        let driver = driver(&connection, id)?.ok_or(ResourceAuthorityError::DriverNotFound)?;
        load_driver_gateway_endpoint_proof(&connection, driver)
    }

    /// Reads the durable endpoint proof for one Resource/Ledger account.
    ///
    /// # Errors
    /// Fails for an unknown account, missing/corrupt proof, or storage error.
    pub fn inspect_resource_ledger_endpoint_proof(
        &self,
        id: ResourceAccountId,
    ) -> Result<ResourceLedgerEndpointProof, ResourceAuthorityError> {
        let connection = self.lock()?;
        if account(&connection, id)?.is_none() {
            return Err(ResourceAuthorityError::AccountNotFound);
        }
        load_resource_ledger_endpoint_proof(&connection, id)
    }
    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ResourceAuthorityError> {
        self.connection
            .lock()
            .map_err(|_| ResourceAuthorityError::LockPoisoned)
    }
}

fn insert_driver_generation(c: &Connection, r: DriverRecord) -> Result<(), ResourceAuthorityError> {
    c.execute(
        "INSERT INTO driver_generations VALUES(?1,?2,?3,?4,?5)",
        params![
            r.driver_id.as_bytes().as_slice(),
            eg(r.generation)?,
            r.fencing_token.as_slice(),
            r.profile_digest.as_slice(),
            eu(r.created_at_ms)?
        ],
    )?;
    Ok(())
}
fn insert_initial_driver_gateway_endpoint(
    c: &Connection,
    r: DriverRecord,
) -> Result<(), ResourceAuthorityError> {
    c.execute(
        "INSERT INTO driver_gateway_identities VALUES(?1, randomblob(16))",
        [r.driver_id.as_bytes().as_slice()],
    )?;
    insert_driver_gateway_generation_proof(c, r)
}
fn insert_driver_gateway_generation_proof(
    c: &Connection,
    r: DriverRecord,
) -> Result<(), ResourceAuthorityError> {
    let changed = c.execute(
        "INSERT INTO driver_gateway_endpoint_proofs
            (driver_id, driver_generation, participant_id, admission_receipt_id)
         SELECT ?1, ?2, participant_id, randomblob(16)
         FROM driver_gateway_identities WHERE driver_id=?1",
        params![r.driver_id.as_bytes().as_slice(), eg(r.generation)?],
    )?;
    if changed != 1 {
        return Err(ResourceAuthorityError::CorruptRecord(
            "driver gateway identity absent",
        ));
    }
    Ok(())
}
fn insert_resource_ledger_endpoint(
    c: &Connection,
    id: ResourceAccountId,
) -> Result<(), ResourceAuthorityError> {
    c.execute(
        "INSERT INTO resource_ledger_endpoint_proofs
            (account_id, participant_id, participant_generation, admission_receipt_id)
         VALUES(?1, randomblob(16), ?2, randomblob(16))",
        params![id.as_bytes().as_slice(), 1_u64.to_be_bytes().as_slice()],
    )?;
    Ok(())
}
fn load_driver_gateway_endpoint_proof(
    c: &Connection,
    driver: DriverRecord,
) -> Result<DriverGatewayEndpointProof, ResourceAuthorityError> {
    let (participant_id, receipt_id) = c.query_row(
        "SELECT participant_id, admission_receipt_id
         FROM driver_gateway_endpoint_proofs
         WHERE driver_id=?1 AND driver_generation=?2",
        params![
            driver.driver_id.as_bytes().as_slice(),
            eg(driver.generation)?
        ],
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
    )?;
    Ok(DriverGatewayEndpointProof {
        driver_id: driver.driver_id,
        participant_id: TaskParticipantId::from_bytes(a16(participant_id)?),
        participant_generation: driver.generation,
        admission_receipt_id: ReceiptId::from_bytes(a16(receipt_id)?),
    })
}
fn load_resource_ledger_endpoint_proof(
    c: &Connection,
    id: ResourceAccountId,
) -> Result<ResourceLedgerEndpointProof, ResourceAuthorityError> {
    let (participant_id, generation, receipt_id) = c.query_row(
        "SELECT participant_id, participant_generation, admission_receipt_id
         FROM resource_ledger_endpoint_proofs WHERE account_id=?1",
        [id.as_bytes().as_slice()],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        },
    )?;
    Ok(ResourceLedgerEndpointProof {
        account_id: id,
        participant_id: TaskParticipantId::from_bytes(a16(participant_id)?),
        participant_generation: generation_from_blob(generation)?,
        admission_receipt_id: ReceiptId::from_bytes(a16(receipt_id)?),
    })
}
fn active_driver(
    c: &Connection,
    id: DriverId,
    g: Generation,
    t: FencingToken,
) -> Result<DriverRecord, ResourceAuthorityError> {
    let r = driver(c, id)?.ok_or(ResourceAuthorityError::DriverNotFound)?;
    if r.generation != g || r.fencing_token != t {
        return Err(ResourceAuthorityError::StaleDriver);
    }
    Ok(r)
}
fn driver(c: &Connection, id: DriverId) -> Result<Option<DriverRecord>, ResourceAuthorityError> {
    let x=c.query_row("SELECT device_id,current_generation,current_fencing_token,profile_digest,created_at_ms FROM drivers WHERE driver_id=?1",[id.as_bytes().as_slice()],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,i64>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,Vec<u8>>(3)?,r.get::<_,i64>(4)?))).optional()?;
    x.map(|x| {
        Ok(DriverRecord {
            driver_id: id,
            device_id: DeviceId::from_bytes(a16(x.0)?),
            generation: dg(x.1)?,
            fencing_token: a32(x.2)?,
            profile_digest: a32(x.3)?,
            created_at_ms: du(x.4)?,
        })
    })
    .transpose()
}
fn driver_generation(
    c: &Connection,
    id: DriverId,
    g: Generation,
) -> Result<DriverRecord, ResourceAuthorityError> {
    let d = driver(c, id)?.ok_or(ResourceAuthorityError::DriverNotFound)?;
    let x=c.query_row("SELECT fencing_token,profile_digest,created_at_ms FROM driver_generations WHERE driver_id=?1 AND generation=?2",params![id.as_bytes().as_slice(),eg(g)?],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,i64>(2)?)))?;
    Ok(DriverRecord {
        generation: g,
        fencing_token: a32(x.0)?,
        profile_digest: a32(x.1)?,
        created_at_ms: du(x.2)?,
        ..d
    })
}
fn driver_by_key(
    c: &Connection,
    k: IdempotencyKey,
) -> Result<Option<DriverRecord>, ResourceAuthorityError> {
    let x = c
        .query_row(
            "SELECT driver_id FROM drivers WHERE idempotency_key=?1",
            [k.as_bytes().as_slice()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    x.map(|x| driver(c, DriverId::from_bytes(a16(x)?)))
        .transpose()
        .map(Option::flatten)
}
fn account(
    c: &Connection,
    id: ResourceAccountId,
) -> Result<Option<AccountRecord>, ResourceAuthorityError> {
    let x=c.query_row("SELECT initial_credit,available_credit,created_at_ms FROM resource_accounts WHERE account_id=?1",[id.as_bytes().as_slice()],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?))).optional()?;
    x.map(|x| {
        Ok(AccountRecord {
            account_id: id,
            initial_credit: du(x.0)?,
            available_credit: du(x.1)?,
            created_at_ms: du(x.2)?,
        })
    })
    .transpose()
}
fn account_by_key(
    c: &Connection,
    k: IdempotencyKey,
) -> Result<Option<AccountRecord>, ResourceAuthorityError> {
    let x = c
        .query_row(
            "SELECT account_id FROM resource_accounts WHERE idempotency_key=?1",
            [k.as_bytes().as_slice()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    x.map(|x| account(c, ResourceAccountId::from_bytes(a16(x)?)))
        .transpose()
        .map(Option::flatten)
}
fn quote(c: &Connection, id: QuoteId) -> Result<Option<QuoteRecord>, ResourceAuthorityError> {
    let x=c.query_row("SELECT driver_id,device_id,driver_generation,driver_fencing_token,operation_proposal_digest,pricing_version,upper_bound,valid_until_ms,created_at_ms FROM quotes WHERE quote_id=?1",[id.as_bytes().as_slice()],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,i64>(2)?,r.get::<_,Vec<u8>>(3)?,r.get::<_,Vec<u8>>(4)?,r.get::<_,Vec<u8>>(5)?,r.get::<_,i64>(6)?,r.get::<_,i64>(7)?,r.get::<_,i64>(8)?))).optional()?;
    x.map(|x| {
        Ok(QuoteRecord {
            quote_id: id,
            driver_id: DriverId::from_bytes(a16(x.0)?),
            device_id: DeviceId::from_bytes(a16(x.1)?),
            driver_generation: dg(x.2)?,
            driver_fencing_token: a32(x.3)?,
            operation_proposal_digest: a32(x.4)?,
            pricing_version: a32(x.5)?,
            upper_bound: du(x.6)?,
            valid_until_ms: du(x.7)?,
            created_at_ms: du(x.8)?,
        })
    })
    .transpose()
}
fn quote_by_key(
    c: &Connection,
    k: IdempotencyKey,
) -> Result<Option<QuoteRecord>, ResourceAuthorityError> {
    let x = c
        .query_row(
            "SELECT quote_id FROM quotes WHERE idempotency_key=?1",
            [k.as_bytes().as_slice()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    x.map(|x| quote(c, QuoteId::from_bytes(a16(x)?)))
        .transpose()
        .map(Option::flatten)
}
fn quote_matches(r: QuoteRecord, q: CreateQuoteRequest) -> bool {
    r.driver_id == q.driver_id
        && r.driver_generation == q.driver_generation
        && r.driver_fencing_token == q.driver_fencing_token
        && r.operation_proposal_digest == q.operation_proposal_digest
        && r.pricing_version == q.pricing_version
        && r.upper_bound == q.upper_bound
        && r.valid_until_ms == q.valid_until_ms
}
fn reservation(
    c: &Connection,
    id: ReservationId,
) -> Result<Option<ReservationRecord>, ResourceAuthorityError> {
    type R = (
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        Vec<u8>,
        i64,
        Vec<u8>,
        i64,
        Vec<u8>,
        i64,
        i64,
        Option<Vec<u8>>,
    );
    let x:R=match c.query_row("SELECT account_id,quote_id,call_id,operation_id,driver_id,device_id,driver_generation,driver_fencing_token,upper_bound,activation_token,state,created_at_ms,activation_receipt_id FROM reservations WHERE reservation_id=?1",[id.as_bytes().as_slice()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?,r.get(10)?,r.get(11)?,r.get(12)?))).optional()?{Some(x)=>x,None=>return Ok(None)};
    Ok(Some(ReservationRecord {
        reservation_id: id,
        account_id: ResourceAccountId::from_bytes(a16(x.0)?),
        quote_id: QuoteId::from_bytes(a16(x.1)?),
        call_id: CallId::from_bytes(a16(x.2)?),
        operation_id: OperationId::from_bytes(a16(x.3)?),
        driver_id: DriverId::from_bytes(a16(x.4)?),
        device_id: DeviceId::from_bytes(a16(x.5)?),
        driver_generation: dg(x.6)?,
        driver_fencing_token: a32(x.7)?,
        upper_bound: du(x.8)?,
        activation_token: a32(x.9)?,
        state: match x.10 {
            0 => ReservationState::Reserved,
            1 => ReservationState::Active,
            _ => return Err(ResourceAuthorityError::CorruptRecord("reservation state")),
        },
        created_at_ms: du(x.11)?,
        activation_receipt_id: x.12.map(a16).transpose()?.map(ReceiptId::from_bytes),
    }))
}
fn reservation_by_key(
    c: &Connection,
    k: IdempotencyKey,
) -> Result<Option<ReservationRecord>, ResourceAuthorityError> {
    let x = c
        .query_row(
            "SELECT reservation_id FROM reservations WHERE idempotency_key=?1",
            [k.as_bytes().as_slice()],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    x.map(|x| reservation(c, ReservationId::from_bytes(a16(x)?)))
        .transpose()
        .map(Option::flatten)
}
fn activation_receipt(
    c: &Connection,
    id: ReservationId,
) -> Result<Option<ActivationReceipt>, ResourceAuthorityError> {
    let x=c.query_row("SELECT receipt_id,operation_id,activated_at_ms FROM reservation_activation_receipts WHERE reservation_id=?1",[id.as_bytes().as_slice()],|r|Ok((r.get::<_,Vec<u8>>(0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,i64>(2)?))).optional()?;
    x.map(|x| {
        Ok(ActivationReceipt {
            receipt_id: ReceiptId::from_bytes(a16(x.0)?),
            reservation_id: id,
            operation_id: OperationId::from_bytes(a16(x.1)?),
            activated_at_ms: du(x.2)?,
        })
    })
    .transpose()
}
fn hash(tag: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update((tag.len() as u64).to_be_bytes());
    h.update(tag);
    for p in parts {
        h.update((p.len() as u64).to_be_bytes());
        h.update(p);
    }
    h.finalize().into()
}
fn id16(t: &[u8], p: &[&[u8]]) -> [u8; 16] {
    hash(t, p)[..16].try_into().expect("fixed")
}
fn eu(v: u64) -> Result<i64, ResourceAuthorityError> {
    i64::try_from(v).map_err(|_| ResourceAuthorityError::CorruptRecord("u64 exceeds i64"))
}
fn du(v: i64) -> Result<u64, ResourceAuthorityError> {
    u64::try_from(v).map_err(|_| ResourceAuthorityError::CorruptRecord("negative integer"))
}
fn eg(v: Generation) -> Result<i64, ResourceAuthorityError> {
    eu(v.get())
}
fn dg(v: i64) -> Result<Generation, ResourceAuthorityError> {
    NonZeroU64::new(du(v)?)
        .map(Generation::new)
        .ok_or(ResourceAuthorityError::CorruptRecord("zero generation"))
}
fn generation_from_blob(v: Vec<u8>) -> Result<Generation, ResourceAuthorityError> {
    let bytes: [u8; 8] = v
        .try_into()
        .map_err(|_| ResourceAuthorityError::CorruptRecord("generation length"))?;
    NonZeroU64::new(u64::from_be_bytes(bytes))
        .map(Generation::new)
        .ok_or(ResourceAuthorityError::CorruptRecord("zero generation"))
}
fn a16(v: Vec<u8>) -> Result<[u8; 16], ResourceAuthorityError> {
    v.try_into()
        .map_err(|_| ResourceAuthorityError::CorruptRecord("id length"))
}
fn a32(v: Vec<u8>) -> Result<[u8; 32], ResourceAuthorityError> {
    v.try_into()
        .map_err(|_| ResourceAuthorityError::CorruptRecord("token length"))
}
