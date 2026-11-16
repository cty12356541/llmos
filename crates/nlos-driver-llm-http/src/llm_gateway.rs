//! Controlled, observed LLM gateway for a trusted runtime embedding.
//!
//! Each `ResourceAuthority` directory owns one gateway journal. The host must
//! protect both stores, provision dedicated accounts through this instance,
//! resolve capability slots and authenticate callers before passing a binding.
//! Do not pass account bindings, child delegation or policy from wire data.
//! The returned usage represents observed single-call gateway debits; provider
//! failover, unobservable usage and external spending can cost more.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nlos_capability::service_call::{
    AuthorizationError, CallBinding, DenyAllAuthorizer, RuntimeCaller, ServiceAction,
    ServiceAuthorizer, ServiceMethod, ServiceObject, ServiceTarget, ServiceTime, authorize_call,
};
use nlos_resource::{
    ConsumeLlmTokensRequest, CreateAccountRequest, CreateLlmTokenBudgetRequest, LlmBudgetDimension,
    LlmTokenBudgetRecord, ResourceAuthority, ResourceAuthorityError,
};
use nlos_schema::sabi::v1::{
    AttenuateBudgetRequest, AttenuateBudgetResult, LlmChatRequest, LlmChatResult, LlmUsageWire,
    QueryBudgetRequest, QueryBudgetResult, RecordExternalUsageRequest, RecordExternalUsageResult,
};
use nlos_schema::{
    CompatibilityError, decode_llm_chat_result, encode_attenuate_budget_request,
    encode_attenuate_budget_result, encode_llm_chat_request, encode_llm_chat_result,
    encode_query_budget_request, encode_query_budget_result, encode_record_external_usage_request,
    encode_record_external_usage_result, llm_gateway_schema_identity,
};
use nlos_types::{
    ApplicationId, Generation, NamespaceId, OperationId, PrincipalId, ReceiptId, ResourceAccountId,
};
use sha2::{Digest, Sha256};

use crate::error::LlmDriverError;
use crate::json::JsonValue;
use crate::shim::{
    LlmChatMessage, LlmChatRequest as ProviderRequest, LlmChatRole, LlmEnforcementShim,
    MeteredBudgetContext, derive_idempotency_key, derive_operation_id, extract_chat_text_fields,
    format_receipt_id_hex,
};

use super::gateway_pricing::{Five, valid_model};
pub use super::gateway_pricing::{
    MAX_MODEL_OUTPUT_TOKENS, ModelAccounting, ModelPrice, PriceRegistry,
};
use super::gateway_state::{
    AccountBinding, Advisory, Chat, Event, MAX_ATTEMPTS, Observed, Phase, Prepared, Provision,
    ProvisionStatus, Rejection, Scope, State, Stop,
};
use super::reservation_journal::{Capacity, Journal};

#[path = "gateway_wire.rs"]
mod wire;
use wire::method_name;

pub const SERVICE_NAME: &str = "llm_gateway";
const MAX_ACTIVE: usize = 32;
const MAX_PER_PRINCIPAL: usize = 4;
const MAX_ADVISORY_ROWS: usize = 256;
const MAX_CHILDREN: usize = 16;
const MAX_ACCOUNTS: usize = 4096;
const MAX_ADMISSION_BYTES: u64 = 192 * 1024 * 1024;
const MAX_ADMISSION_RECORDS: u64 = 196_608;
const MAX_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_JOURNAL_RECORDS: u64 = 262_144;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GatewayLimits {
    pub active_calls: usize,
    pub calls_per_principal: usize,
    pub advisory_rows_per_window: usize,
    pub children_per_parent: usize,
    pub active_accounts: usize,
    pub journal_admission_bytes: u64,
    pub journal_admission_records: u64,
    pub journal_max_bytes: u64,
    pub journal_max_records: u64,
}

impl Default for GatewayLimits {
    fn default() -> Self {
        Self {
            active_calls: MAX_ACTIVE,
            calls_per_principal: MAX_PER_PRINCIPAL,
            advisory_rows_per_window: MAX_ADVISORY_ROWS,
            children_per_parent: MAX_CHILDREN,
            active_accounts: MAX_ACCOUNTS,
            journal_admission_bytes: MAX_ADMISSION_BYTES,
            journal_admission_records: MAX_ADMISSION_RECORDS,
            journal_max_bytes: MAX_JOURNAL_BYTES,
            journal_max_records: MAX_JOURNAL_RECORDS,
        }
    }
}

impl GatewayLimits {
    fn verify(self) -> Result<(), GatewayError> {
        if !(1..=MAX_ACTIVE).contains(&self.active_calls)
            || !(1..=MAX_PER_PRINCIPAL).contains(&self.calls_per_principal)
            || !(1..=MAX_ADVISORY_ROWS).contains(&self.advisory_rows_per_window)
            || !(1..=MAX_CHILDREN).contains(&self.children_per_parent)
            || !(1..=MAX_ACCOUNTS).contains(&self.active_accounts)
            || !(1..=MAX_ADMISSION_BYTES).contains(&self.journal_admission_bytes)
            || !(1..=MAX_ADMISSION_RECORDS).contains(&self.journal_admission_records)
            || !self.admission().fits(self.maximum())
            || !self
                .maximum()
                .fits(Capacity::new(MAX_JOURNAL_BYTES, MAX_JOURNAL_RECORDS))
        {
            return Err(GatewayError::InvalidConfiguration);
        }
        Ok(())
    }

    const fn admission(self) -> Capacity {
        Capacity::new(self.journal_admission_bytes, self.journal_admission_records)
    }

    const fn maximum(self) -> Capacity {
        Capacity::new(self.journal_max_bytes, self.journal_max_records)
    }
}

/// All fields represent trusted host configuration.
pub struct GatewayConfig {
    pub shim: LlmEnforcementShim,
    pub prices: PriceRegistry,
    pub limits: GatewayLimits,
    /// `None` denies every service call. Policy and clock callbacks run
    /// synchronously and must not reenter the gateway.
    pub authorizer: Option<Arc<dyn ServiceAuthorizer>>,
    pub time: Arc<dyn Fn() -> Result<ServiceTime, AuthorizationError> + Send + Sync>,
}

/// Scope the host requests when provisioning a dedicated root account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RootAccountBinding {
    pub principal: PrincipalId,
    pub application: ApplicationId,
    pub namespace: NamespaceId,
    pub authorization_domain: [u8; 16],
    pub generation: Generation,
    pub valid_until_ms: u64,
}

/// Account activation from the protected gateway journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GatewayAccountBinding {
    account: ResourceAccountId,
    binding: AccountBinding,
}

impl GatewayAccountBinding {
    #[must_use]
    pub const fn account_id(self) -> ResourceAccountId {
        self.account
    }

    #[must_use]
    pub const fn parent(self) -> Option<ResourceAccountId> {
        self.binding.parent
    }

    #[must_use]
    pub const fn valid_until_ms(self) -> u64 {
        self.binding.valid_until_ms
    }

    fn target(self) -> ServiceTarget {
        ServiceTarget {
            namespace: self.binding.namespace,
            object: ServiceObject::GatewayAccount(self.account),
            generation: self.binding.generation,
        }
    }

    fn accepts(self, caller: &RuntimeCaller) -> bool {
        self.binding.principal == caller.principal
            && self.binding.application == caller.application
            && self
                .binding
                .recipient
                .is_none_or(|binding| binding == *caller)
    }
}

/// Child recipient authenticated by the runtime. This is an out-of-band
/// assertion from the trusted host. For a different principal or application
/// the host must have explicitly authorized delegation to this recipient.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildSessionBinding {
    pub recipient: RuntimeCaller,
    pub stable_identity: [u8; 16],
    pub delegated: bool,
}

/// The activated authority account and its parent edge for the trusted host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChildBudgetBinding {
    pub account: GatewayAccountBinding,
    pub receipt: ReceiptId,
}

pub struct ChatReply {
    pub result: LlmChatResult,
    pub operation: OperationId,
    pub receipt: ReceiptId,
    pub provider_attempts: u8,
    pub price_version: u64,
}

pub struct AttenuationReply {
    pub result: AttenuateBudgetResult,
    pub child: ChildBudgetBinding,
}

pub struct AdvisoryReply {
    pub result: RecordExternalUsageResult,
    pub receipt: ReceiptId,
}

pub struct WireReply {
    pub frame: Vec<u8>,
    pub child: Option<ChildBudgetBinding>,
}

impl fmt::Debug for ChatReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChatReply")
            .field("operation", &self.operation)
            .field("receipt", &self.receipt)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GatewayError {
    Authorization(AuthorizationError),
    InvalidRequest,
    InvalidConfiguration,
    IncompatibleSchema,
    UnsupportedMethod,
    UnsupportedModel,
    UnsupportedBinding,
    FrameTooLarge,
    Arithmetic,
    Budget(LlmBudgetDimension),
    Quota,
    Conflict,
    Durability,
    WriterBusy,
    UsageUnknown,
    ProviderFailure,
    EffectUnknown(OperationId),
    SettlementRejected(OperationId),
}

impl fmt::Display for GatewayError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Authorization(_) => "gateway authorization failed",
            Self::InvalidRequest => "invalid gateway request",
            Self::InvalidConfiguration => "invalid gateway configuration",
            Self::IncompatibleSchema => "incompatible gateway schema",
            Self::UnsupportedMethod => "gateway method is unsupported",
            Self::UnsupportedModel => "gateway model has no supported price",
            Self::UnsupportedBinding => "gateway execution binding is unsupported",
            Self::FrameTooLarge => "gateway payload exceeds 65536 bytes",
            Self::Arithmetic => "gateway usage cannot be represented",
            Self::Budget(_) => "gateway budget exhausted",
            Self::Quota => "gateway quota exhausted",
            Self::Conflict => "gateway idempotency conflict",
            Self::Durability => "gateway durable state unavailable",
            Self::WriterBusy => "gateway journal is already in use",
            Self::UsageUnknown => "provider usage is unknown",
            Self::ProviderFailure => "provider call failed",
            Self::EffectUnknown(_) => "provider effect requires reconciliation",
            Self::SettlementRejected(_) => "observed usage exceeds the gateway envelope",
        })
    }
}

impl Error for GatewayError {}

impl From<AuthorizationError> for GatewayError {
    fn from(error: AuthorizationError) -> Self {
        Self::Authorization(error)
    }
}

impl From<CompatibilityError> for GatewayError {
    fn from(error: CompatibilityError) -> Self {
        match error {
            CompatibilityError::FrameTooLarge { .. } => Self::FrameTooLarge,
            error if error.is_abi_version_error() => Self::IncompatibleSchema,
            _ => Self::InvalidRequest,
        }
    }
}

/// Gateway operator's sanitized lookup for an already known operation ID.
/// Exposure to applications requires the host's separate operation authorization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reconciliation {
    pub quote: LlmUsageWire,
    pub price_version: u64,
    pub observed: Option<LlmUsageWire>,
    pub reasoning_tokens: Option<u64>,
    pub provider_response_fingerprint: Option<[u8; 32]>,
    /// Calls for which a transport attempt was initiated. None after a crash
    /// that could have occurred on either side of the network write.
    pub provider_attempts: Option<u8>,
    pub durable_attempt_fences: u8,
    pub status: ReconciliationStatus,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconciliationStatus {
    Pending,
    Observed,
    Settled(ReceiptId),
    Abandoned,
    EffectUnknown,
    OverQuote,
    OverAuthorityEnvelope,
    OversizedResult,
}

/// Independently authorized four-method gateway backed by one journal writer.
///
/// A checked policy snapshot cannot serve as a credential for a later call.
///
/// ```compile_fail,E0308
/// use nlos_capability::service_call::AuthorizedServiceCall;
/// use nlos_driver_llm_http::llm_gateway::{Gateway, GatewayAccountBinding};
/// use nlos_schema::sabi::v1::QueryBudgetRequest;
///
/// fn query(gateway: &Gateway, grant: &AuthorizedServiceCall, account: &GatewayAccountBinding, request: &QueryBudgetRequest) {
///     let _ = gateway.query_budget(grant, account, request);
/// }
/// ```
pub struct Gateway {
    resource: ResourceAuthority,
    store: Mutex<Store>,
    config: GatewayConfig,
    last_time: Mutex<ServiceTime>,
}

struct Store {
    state: State,
    journal: Journal,
    limits: GatewayLimits,
    #[cfg(test)]
    fault: Option<FaultPoint>,
}

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum FaultPoint {
    Prepared,
    Fence,
    Observed,
    ReadyForConsume,
    Consumed,
    Settled,
    ChildIntent,
    ChildAccount,
    ChildBudget,
}

impl Store {
    fn append(&mut self, event: Event) -> Result<(), GatewayError> {
        let result = self.append_inner(event);
        if result.is_err_and(|error| error != GatewayError::Quota) {
            self.journal.poison();
        }
        result
    }

    fn append_inner(&mut self, event: Event) -> Result<(), GatewayError> {
        let capacity = self.state.next_hold(&event)?;
        let admission = matches!(
            &event,
            Event::Prepare(..) | Event::Provision(..) | Event::Advisory(..)
        );
        self.journal.append(
            &event.encode()?,
            capacity,
            admission.then(|| self.limits.admission()),
            self.limits.maximum(),
        )?;
        self.state.apply(event)
    }

    #[cfg(test)]
    fn fail_at(&mut self, point: FaultPoint) -> Result<(), GatewayError> {
        if self.fault == Some(point) {
            self.journal.poison();
            Err(GatewayError::Durability)
        } else {
            Ok(())
        }
    }
}

struct GatewayContext<'a> {
    budget: MeteredBudgetContext<'a>,
    raw: Scope,
}

impl<'a> GatewayContext<'a> {
    fn new(resource: &'a ResourceAuthority, raw: Scope, digest: &[u8; 32], now_ms: u64) -> Self {
        let parts = [
            raw.account.as_bytes().as_slice(),
            raw.raw_key.as_bytes(),
            digest,
        ];
        Self {
            budget: MeteredBudgetContext {
                resource,
                account_id: raw.account,
                operation_id: derive_operation_id(b"nlos/llm-gateway/chat-operation/v1", &parts),
                idempotency_key: derive_idempotency_key(
                    b"nlos/llm-gateway/chat-consume/v1",
                    &parts,
                ),
                now_ms,
            },
            raw,
        }
    }
}

impl Gateway {
    /// Initializes a new journal in a precreated, host-protected authority directory.
    ///
    /// # Errors
    /// Fails on invalid configuration, clock or durability errors, or an existing journal.
    pub fn initialize(root: impl AsRef<Path>, config: GatewayConfig) -> Result<Self, GatewayError> {
        Self::load(root.as_ref(), config, true)
    }

    /// Opens an existing store under an exclusive, OS-backed writer lock and
    /// reconciles stored settlement and provisioning intents.
    ///
    /// # Errors
    /// Fails on lock contention, invalid configuration, clock or durability errors.
    pub fn open(root: impl AsRef<Path>, config: GatewayConfig) -> Result<Self, GatewayError> {
        Self::load(root.as_ref(), config, false)
    }

    fn load(root: &Path, config: GatewayConfig, initialize: bool) -> Result<Self, GatewayError> {
        config.limits.verify()?;
        let now = (config.time)()?;
        let root = root.canonicalize().map_err(|_| GatewayError::Durability)?;
        let mut state = State::default();
        let journal = Journal::open(&root, initialize, config.limits.maximum(), |bytes| {
            state
                .apply(Event::decode(bytes)?)
                .map_err(|_| GatewayError::Durability)
        })?;
        journal.check_hold(state.reserved, config.limits.maximum())?;
        // An accidentally removed authority DB must not be recreated during replay.
        if !initialize && !root.join("resource-authority.db").is_file() {
            return Err(GatewayError::Durability);
        }
        let resource = ResourceAuthority::open(root).map_err(|_| GatewayError::Durability)?;
        let limits = config.limits;
        let gateway = Self {
            resource,
            config,
            last_time: Mutex::new(ServiceTime {
                wall_ms: state.wall_floor.max(now.wall_ms),
                monotonic_ns: now.monotonic_ns,
            }),
            store: Mutex::new(Store {
                state,
                journal,
                limits,
                #[cfg(test)]
                fault: None,
            }),
        };
        {
            let mut store = gateway.lock()?;
            gateway.recover(&mut store, gateway.time()?, true)?;
        }
        Ok(gateway)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Store>, GatewayError> {
        let store = self.store.lock().map_err(|_| GatewayError::Durability)?;
        store.journal.check_ready()?;
        Ok(store)
    }

    fn time(&self) -> Result<ServiceTime, GatewayError> {
        // Authorization may already hold the Store lock. Serialize sampling and
        // both clock floors independently of Store.
        let mut last = self
            .last_time
            .lock()
            .map_err(|_| AuthorizationError::TimeUnavailable)?;
        let now = (self.config.time)()?;
        if now.wall_ms < last.wall_ms || now.monotonic_ns < last.monotonic_ns {
            return Err(AuthorizationError::TimeUnavailable.into());
        }
        *last = now;
        Ok(now)
    }

    fn authorize(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        method: ServiceMethod,
        input_digest: [u8; 32],
    ) -> Result<(ServiceTime, u64), GatewayError> {
        if !account.accepts(&call.caller) {
            return Err(AuthorizationError::Denied.into());
        }
        let action = ServiceAction {
            method,
            target: account.target(),
            input_digest,
        };
        let now = self.time()?;
        let authorizer = self
            .config
            .authorizer
            .as_deref()
            .unwrap_or(&DenyAllAuthorizer);
        let grant = authorize_call(authorizer, call, &action, now)?;
        if now.wall_ms > account.valid_until_ms() {
            return Err(AuthorizationError::GrantExpired.into());
        }
        Ok((now, grant.valid_until_ms()))
    }

    fn account<'a>(
        store: &'a Store,
        supplied: &GatewayAccountBinding,
        now: ServiceTime,
    ) -> Result<&'a super::gateway_state::Account, GatewayError> {
        let entry = store
            .state
            .accounts
            .get(&supplied.account)
            .ok_or(AuthorizationError::Denied)?;
        if supplied.binding != entry.binding
            || !store.state.account_is_active(supplied.account, now.wall_ms)
        {
            return Err(AuthorizationError::Denied.into());
        }
        Ok(entry)
    }

    /// Trusted host bootstrap of a dedicated authority account.
    ///
    /// # Errors
    /// Fails on conflicting provisioning, expired bindings and quota or storage errors.
    pub fn provision_root(
        &self,
        root: RootAccountBinding,
        limits: LlmUsageWire,
    ) -> Result<GatewayAccountBinding, GatewayError> {
        let identity = derive_idempotency_key(
            b"nlos/llm-gateway/root-binding/v1",
            &[
                root.principal.as_bytes(),
                root.application.as_bytes(),
                root.namespace.as_bytes(),
                &root.authorization_domain,
            ],
        )
        .into_bytes();
        let binding = AccountBinding {
            principal: root.principal,
            application: root.application,
            namespace: root.namespace,
            domain: root.authorization_domain,
            generation: root.generation,
            valid_until_ms: root.valid_until_ms,
            parent: None,
            recipient: None,
        };
        let now = self.time()?;
        let limits = Five::from(limits);
        let mut store = self.lock()?;
        self.recover(&mut store, now, false)?;
        if root.valid_until_ms < now.wall_ms {
            return Err(AuthorizationError::GrantExpired.into());
        }
        if let Some((prior, status)) = store.state.provisions.get(&identity) {
            if prior.binding != binding || prior.limits != limits {
                return Err(GatewayError::Conflict);
            }
            return match *status {
                ProvisionStatus::Active(account) => Ok(GatewayAccountBinding { account, binding }),
                _ => Err(GatewayError::Durability),
            };
        }
        self.capacity_for_account(&store, None, now.wall_ms)?;
        let provision = Provision {
            identity,
            caller: None,
            binding,
            limits,
            created_at_ms: now.wall_ms,
        };
        Self::validate_provision(&provision)?;
        store.append(Event::Provision(provision))?;
        self.finish_provision(&mut store, &provision, now)
    }

    fn validate_provision(provision: &Provision) -> Result<(), GatewayError> {
        if !provision.limits.authority_sized() || i64::try_from(provision.created_at_ms).is_err() {
            return Err(GatewayError::Arithmetic);
        }
        Ok(())
    }

    fn capacity_for_account(
        &self,
        store: &Store,
        parent: Option<ResourceAccountId>,
        now_ms: u64,
    ) -> Result<(), GatewayError> {
        let mut total = 0;
        let mut children = 0;
        for (&id, account) in &store.state.accounts {
            if account.binding.valid_until_ms < now_ms || !store.state.account_is_active(id, now_ms)
            {
                continue;
            }
            total += 1;
            children += usize::from(parent.is_some() && account.binding.parent == parent);
            if total >= self.config.limits.active_accounts
                || children >= self.config.limits.children_per_parent
            {
                return Err(GatewayError::Quota);
            }
        }
        Ok(())
    }

    fn finish_provision(
        &self,
        store: &mut Store,
        provision: &Provision,
        now: ServiceTime,
    ) -> Result<GatewayAccountBinding, GatewayError> {
        if now.wall_ms > provision.binding.valid_until_ms {
            store.append(Event::ExpireProvision(provision.identity))?;
            return Err(AuthorizationError::GrantExpired.into());
        }
        #[cfg(test)]
        store.fail_at(FaultPoint::ChildIntent)?;
        let account_key = derive_idempotency_key(
            b"nlos/llm-gateway/authority-account/v1",
            &[&provision.identity],
        );
        let budget_key = derive_idempotency_key(
            b"nlos/llm-gateway/authority-budget/v1",
            &[&provision.identity],
        );
        let attempt = || {
            self.resource
                .create_account(CreateAccountRequest {
                    initial_credit: 0,
                    idempotency_key: account_key,
                    created_at_ms: provision.created_at_ms,
                })
                .map_err(|_| GatewayError::Durability)
        };
        let account = match attempt() {
            Ok(account) => account,
            Err(error) => {
                store.journal.poison();
                return Err(error);
            }
        };
        #[cfg(test)]
        store.fail_at(FaultPoint::ChildAccount)?;
        let decision = self
            .resource
            .create_llm_token_budget(CreateLlmTokenBudgetRequest {
                account_id: account.account_id,
                limits: provision.limits.limits(),
                idempotency_key: budget_key,
                created_at_ms: provision.created_at_ms,
            });
        if !decision.is_ok_and(|data| Five::from_limits(data.record().limits) == provision.limits) {
            store.journal.poison();
            return Err(GatewayError::Durability);
        }
        #[cfg(test)]
        store.fail_at(FaultPoint::ChildBudget)?;
        store.append(Event::Activate(provision.identity, account.account_id))?;
        Ok(GatewayAccountBinding {
            account: account.account_id,
            binding: provision.binding,
        })
    }

    fn inspect(&self, account: ResourceAccountId) -> Result<LlmTokenBudgetRecord, GatewayError> {
        self.resource
            .inspect_llm_token_budget(account)
            .map_err(|_| GatewayError::Durability)
    }

    fn effective_remaining(
        &self,
        store: &Store,
        account: ResourceAccountId,
    ) -> Result<(Five, Five, Five, u64), GatewayError> {
        self.remaining_excluding(store, account, Five::default())
    }

    fn remaining_excluding(
        &self,
        store: &Store,
        account: ResourceAccountId,
        excluded: Five,
    ) -> Result<(Five, Five, Five, u64), GatewayError> {
        let mut minimum = Five([u64::MAX; 5]);
        let mut own = None;
        for id in store.state.chain(account)? {
            let entry = &store.state.accounts[&id];
            let authority = self.inspect(id)?;
            let limit = entry.limits.min(Five::from_limits(authority.limits));
            let consumed = Five::consumed(&authority).add(entry.settled_below)?;
            let pending = entry
                .pending
                .sub(excluded)
                .map_err(|_| GatewayError::Durability)?;
            let remaining = limit.sub(consumed)?.sub(pending)?;
            minimum = minimum.min(remaining);
            if id == account {
                own = Some((limit, consumed, authority.created_at_ms));
            }
        }
        let (limit, consumed, created) = own.ok_or(GatewayError::Durability)?;
        Ok((limit, consumed, minimum, created))
    }
}

impl Gateway {
    /// Queries the authorized account, including hard subtree use and advisory use separately.
    ///
    /// # Errors
    /// Fails on invalid input, denied authorization, clock or durability failure.
    pub fn query_budget(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        request: &QueryBudgetRequest,
    ) -> Result<QueryBudgetResult, GatewayError> {
        self.query(call, account, request, &[])
    }

    fn query(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        request: &QueryBudgetRequest,
        header_digest: &[u8],
    ) -> Result<QueryBudgetResult, GatewayError> {
        let method = ServiceMethod::QueryBudget;
        encode_query_budget_request(request)?;
        let digest = check_digest(method, &[], &[], header_digest)?;
        let (now, _) = self.authorize(call, account, method, digest)?;
        let mut store = self.lock()?;
        Self::account(&store, account, now)?;
        self.recover(&mut store, now, false)?;
        let (now, _) = self.authorize(call, account, method, digest)?;
        Self::account(&store, account, now)?;
        let (limit, consumed, remaining, _) = self.effective_remaining(&store, account.account)?;
        let external_advisory = store.state.accounts[&account.account].advisory;
        let result = QueryBudgetResult {
            schema: Some(llm_gateway_schema_identity()),
            limit: Some(limit.wire()),
            consumed: Some(consumed.wire()),
            remaining: Some(remaining.wire()),
            external_advisory: Some(external_advisory.wire()),
        };
        encode_query_budget_result(&result)?;
        Ok(result)
    }

    /// Appends an advisory usage row. The client timestamp is never used as a rate clock.
    ///
    /// # Errors
    /// Fails on invalid input or authorization, dedup conflict, quota or storage errors.
    pub fn record_external_usage(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        request: &RecordExternalUsageRequest,
    ) -> Result<AdvisoryReply, GatewayError> {
        self.advisory(call, account, request, &[])
    }

    fn advisory(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        request: &RecordExternalUsageRequest,
        header_digest: &[u8],
    ) -> Result<AdvisoryReply, GatewayError> {
        let method = ServiceMethod::RecordExternalUsage;
        encode_record_external_usage_request(request)?;
        let mut canonical = request.clone();
        canonical.schema = Some(llm_gateway_schema_identity());
        let digest = check_digest(
            method,
            &encode_record_external_usage_request(&canonical)?,
            &[],
            header_digest,
        )?;
        let (initial, _) = self.authorize(call, account, method, digest)?;
        let scope = raw_scope(call, account)?;
        let usage = request.usage.ok_or(GatewayError::InvalidRequest)?.into();
        if !valid_model(&request.provider) || !valid_model(&request.model) {
            return Err(GatewayError::InvalidRequest);
        }
        let mut store = self.lock()?;
        Self::account(&store, account, initial)?;
        self.recover(&mut store, initial, false)?;
        let (now, _) = self.authorize(call, account, method, digest)?;
        Self::account(&store, account, now)?;
        if let Some(row) = store.state.advisories.get(&scope) {
            compare_digest(row.digest, digest)?;
            return advisory_reply(true, row.receipt);
        }
        if now.wall_ms < store.state.advisory_high_water {
            return Err(AuthorizationError::TimeUnavailable.into());
        }
        store.state.prune_advisory(now.wall_ms);
        if !store
            .state
            .advisory_quota(&call.caller, self.config.limits.advisory_rows_per_window)
        {
            return Err(GatewayError::Quota);
        }
        store.state.accounts[&account.account].advisory.add(usage)?;
        let receipt = ReceiptId::from_bytes(
            derive_idempotency_key(
                b"nlos/llm-gateway/advisory-receipt/v1",
                &[scope.account.as_bytes(), scope.raw_key.as_bytes(), &digest],
            )
            .into_bytes(),
        );
        // Store the immutable semantic digest, never raw provider/model strings.
        store.append(Event::Advisory(Advisory {
            key: scope,
            digest,
            session: call.caller.session,
            observed_wall_ms: now.wall_ms,
            usage,
            receipt,
        }))?;
        advisory_reply(false, receipt)
    }

    /// Provisions and returns a session-bound child through the trusted host.
    ///
    /// # Errors
    /// Fails on missing or unauthorized child binding, insufficient budget, conflict or durability.
    pub fn attenuate_budget(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        child: Option<&ChildSessionBinding>,
        request: &AttenuateBudgetRequest,
    ) -> Result<AttenuationReply, GatewayError> {
        self.attenuate(call, account, child, request, &[])
    }

    fn attenuate(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        child: Option<&ChildSessionBinding>,
        request: &AttenuateBudgetRequest,
        header_digest: &[u8],
    ) -> Result<AttenuationReply, GatewayError> {
        let method = ServiceMethod::AttenuateBudget;
        encode_attenuate_budget_request(request)?;
        let child = child.ok_or(AuthorizationError::Denied)?;
        let mut canonical = request.clone();
        canonical.schema = Some(llm_gateway_schema_identity());
        let recipient = child.recipient;
        let recipient_generation = recipient.process_generation.get().to_be_bytes();
        let delegated = [u8::from(child.delegated)];
        let sidecar = [
            child.stable_identity.as_slice(),
            recipient.principal.as_bytes(),
            recipient.application.as_bytes(),
            recipient.process.as_bytes(),
            recipient_generation.as_slice(),
            recipient.session.as_bytes(),
            &delegated,
        ];
        let digest = check_digest(
            method,
            &encode_attenuate_budget_request(&canonical)?,
            &sidecar,
            header_digest,
        )?;
        let (initial, _) = self.authorize(call, account, method, digest)?;
        if (call.caller.principal != recipient.principal
            || call.caller.application != recipient.application)
            && !child.delegated
        {
            return Err(AuthorizationError::Denied.into());
        }
        let scope = raw_scope(call, account)?;
        let limits = Five::from(request.child_limit.ok_or(GatewayError::InvalidRequest)?);
        let mut store = self.lock()?;
        Self::account(&store, account, initial)?;
        self.recover(&mut store, initial, false)?;
        let (now, grant_until_ms) = self.authorize(call, account, method, digest)?;
        Self::account(&store, account, now)?;
        if let Some(identity) = store.state.child_requests.get(&scope) {
            let (provision, status) = store.state.provisions[identity];
            let (_, expected_digest) = provision.caller.ok_or(GatewayError::Durability)?;
            compare_digest(expected_digest, digest)?;
            if let ProvisionStatus::Active(id) = status
                && store.state.account_is_active(id, now.wall_ms)
            {
                return attenuation_reply(
                    &provision,
                    GatewayAccountBinding {
                        account: id,
                        binding: provision.binding,
                    },
                );
            }
            return Err(AuthorizationError::GrantExpired.into());
        }
        if !(now.wall_ms..=grant_until_ms.min(account.valid_until_ms()))
            .contains(&request.valid_until_ms)
        {
            return Err(AuthorizationError::GrantExpired.into());
        }
        let identity = derive_idempotency_key(
            b"nlos/llm-gateway/child-binding/v1",
            &[
                account.binding.namespace.as_bytes(),
                &account.binding.domain,
                account.account.as_bytes(),
                recipient.principal.as_bytes(),
                recipient.application.as_bytes(),
                &child.stable_identity,
            ],
        )
        .into_bytes();
        if store.state.provisions.contains_key(&identity) {
            return Err(GatewayError::Conflict);
        }
        self.capacity_for_account(&store, Some(account.account), now.wall_ms)?;
        self.effective_remaining(&store, account.account)?
            .2
            .sub(limits)?;
        let binding = AccountBinding {
            principal: recipient.principal,
            application: recipient.application,
            namespace: account.binding.namespace,
            domain: identity,
            generation: Generation::INITIAL,
            valid_until_ms: request.valid_until_ms,
            parent: Some(account.account),
            recipient: Some(recipient),
        };
        let provision = Provision {
            identity,
            caller: Some((scope, digest)),
            binding,
            limits,
            created_at_ms: now.wall_ms,
        };
        Self::validate_provision(&provision)?;
        store.append(Event::Provision(provision))?;
        let activated = self.finish_provision(&mut store, &provision, now)?;
        attenuation_reply(&provision, activated)
    }

    /// Performs an observed, five-dimensional metered chat after durable admission.
    ///
    /// # Errors
    /// Reports typed authorization, admission, provider, reconciliation and durability errors.
    pub async fn llm_chat(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        request: &LlmChatRequest,
    ) -> Result<ChatReply, GatewayError> {
        self.chat(call, account, request, &[]).await
    }

    async fn chat(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        request: &LlmChatRequest,
        header_digest: &[u8],
    ) -> Result<ChatReply, GatewayError> {
        let method = ServiceMethod::LlmChat;
        encode_llm_chat_request(request)?;
        let mut canonical = request.clone();
        canonical.schema = Some(llm_gateway_schema_identity());
        let digest = check_digest(
            method,
            &encode_llm_chat_request(&canonical)?,
            &[],
            header_digest,
        )?;
        let (now, _) = self.authorize(call, account, method, digest)?;
        let scope = raw_scope(call, account)?;
        let admission = {
            let mut store = self.lock()?;
            Self::account(&store, account, now)?;
            self.recover(&mut store, now, false)?;
            if let Some(prior) = store.state.chats.get(&scope) {
                compare_digest(prior.prepared.digest, digest)?;
                ChatAdmission::Replay(prior.clone())
            } else {
                let (admit_at, _) = self.authorize(call, account, method, digest)?;
                Self::account(&store, account, admit_at)?;
                let (prepared, request) =
                    self.prepare_chat(&mut store, call, scope, &canonical, digest, admit_at)?;
                store.state.live.insert(scope);
                ChatAdmission::Ready(prepared, request)
            }
        };
        match admission {
            ChatAdmission::Replay(prior) => {
                self.authorize(call, account, method, digest)?;
                chat_reply(&prior)
            }
            ChatAdmission::Ready(plan, outgoing) => {
                let _live = LiveCall {
                    gateway: self,
                    scope,
                };
                self.execute_chat(call, account, scope, &plan, &outgoing, now.wall_ms)
                    .await?;
                self.authorize(call, account, method, digest)?;
                chat_reply(
                    self.lock()?
                        .state
                        .chats
                        .get(&scope)
                        .ok_or(GatewayError::Durability)?,
                )
            }
        }
    }

    fn prepare_chat(
        &self,
        store: &mut Store,
        call: &CallBinding,
        scope: Scope,
        request: &LlmChatRequest,
        digest: [u8; 32],
        now: ServiceTime,
    ) -> Result<(Prepared, ProviderRequest), GatewayError> {
        if request.turns.is_empty() || request.temperature_milli > 2_000 {
            return Err(GatewayError::InvalidRequest);
        }
        if self
            .config
            .shim
            .contains_provider_credential(&request.model)
        {
            return Err(GatewayError::UnsupportedModel);
        }
        let price = self.config.prices.get(&request.model)?;
        let quote = price.quote(
            request.turns.iter().map(|item| item.content.len()),
            request.max_output_tokens,
        )?;
        let request = provider_request(request)?;
        let (_, _, remaining, created) = self.effective_remaining(store, scope.account)?;
        remaining.sub(quote)?;
        let principal_pending = store
            .state
            .pending
            .iter()
            .filter(|key| {
                store.state.accounts[&key.account].binding.principal == call.caller.principal
            })
            .count();
        if store.state.pending.len() >= self.config.limits.active_calls
            || principal_pending >= self.config.limits.calls_per_principal
        {
            return Err(GatewayError::Quota);
        }
        let context = GatewayContext::new(&self.resource, scope, &digest, now.wall_ms.max(created));
        let deadline = call
            .deadline_monotonic_ns
            .ok_or(AuthorizationError::MissingDeadline)?;
        let time_left = deadline
            .checked_sub(now.monotonic_ns)
            .ok_or(AuthorizationError::DeadlineExpired)?;
        let expires_wall_ms = now
            .wall_ms
            .checked_add(time_left.div_ceil(1_000_000))
            .ok_or(GatewayError::Arithmetic)?;
        if i64::try_from(context.budget.now_ms).is_err() {
            return Err(GatewayError::Arithmetic);
        }
        let prepared = Prepared {
            digest,
            operation: context.budget.operation_id,
            consume_key: context.budget.idempotency_key,
            quote,
            price: price.clone(),
            deadline_monotonic_ns: deadline,
            expires_wall_ms: expires_wall_ms.max(context.budget.now_ms),
            consumed_at_ms: context.budget.now_ms,
        };
        store.append(Event::Prepare(context.raw, prepared.clone()))?;
        #[cfg(test)]
        store.fail_at(FaultPoint::Prepared)?;
        Ok((prepared, request))
    }

    async fn execute_chat(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        scope: Scope,
        plan: &Prepared,
        outgoing: &ProviderRequest,
        now_ms: u64,
    ) -> Result<(), GatewayError> {
        let sticky = format_receipt_id_hex(ReceiptId::from_bytes(
            derive_idempotency_key(
                b"nlos/llm-gateway/provider-affinity/v1",
                &[scope.account.as_bytes()],
            )
            .into_bytes(),
        ));
        let mut attempts = 0;
        let mut denied = None;
        let response = self
            .config
            .shim
            .dispatch_gateway_chat(
                &plan.price.model,
                outgoing,
                &sticky,
                now_ms,
                MAX_ATTEMPTS,
                |attempt| match self.fence_attempt(call, account, scope, plan, attempt) {
                    Ok((started, remaining)) => {
                        attempts = u8::try_from(attempt).unwrap_or(MAX_ATTEMPTS);
                        Ok((started.wall_ms, Some(remaining)))
                    }
                    Err(error) => {
                        denied = Some(error);
                        Err(LlmDriverError::InvalidProviderPayload(
                            "gateway dispatch denied",
                        ))
                    }
                },
            )
            .await;
        if let Some(error) = denied {
            if error == GatewayError::Durability {
                return if attempts == 0 {
                    Err(error)
                } else {
                    Err(GatewayError::EffectUnknown(plan.operation))
                };
            }
            let cause = match error {
                GatewayError::Authorization(AuthorizationError::DeadlineExpired) => Stop::Deadline,
                GatewayError::Authorization(AuthorizationError::TimeUnavailable) => Stop::Clock,
                _ => Stop::Rights,
            };
            self.stop_chat(scope, cause, attempts, None, plan.operation)?;
            return Err(if attempts == 0 {
                error
            } else {
                GatewayError::EffectUnknown(plan.operation)
            });
        }
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let stop = match error {
                    LlmDriverError::HttpTimeout { .. } | LlmDriverError::HttpTransport(_) => {
                        Stop::Transport
                    }
                    _ => Stop::Provider,
                };
                self.stop_chat(scope, stop, attempts, None, plan.operation)?;
                return Err(if attempts == 0 {
                    GatewayError::ProviderFailure
                } else {
                    GatewayError::EffectUnknown(plan.operation)
                });
            }
        };
        let fingerprint = Sha256::digest(&response.body).into();
        let observed = match observed_result(
            &self.config.shim,
            plan,
            &response.body,
            fingerprint,
            attempts,
        ) {
            Ok(observed) => observed,
            Err(error) => {
                let cause = match error {
                    GatewayError::Arithmetic => Stop::Arithmetic,
                    GatewayError::UsageUnknown => Stop::UsageUnknown,
                    _ => Stop::Provider,
                };
                self.stop_chat(scope, cause, attempts, Some(fingerprint), plan.operation)?;
                return Err(GatewayError::EffectUnknown(plan.operation));
            }
        };
        let mut store = self
            .lock()
            .map_err(|_| GatewayError::EffectUnknown(plan.operation))?;
        store
            .append(Event::Observe(scope, observed.clone()))
            .map_err(|_| GatewayError::EffectUnknown(plan.operation))?;
        #[cfg(test)]
        store.fail_at(FaultPoint::Observed)?;
        self.finish_settlement(&mut store, scope, plan, &observed)
            .map_err(|_| GatewayError::EffectUnknown(plan.operation))
    }

    fn fence_attempt(
        &self,
        call: &CallBinding,
        account: &GatewayAccountBinding,
        scope: Scope,
        plan: &Prepared,
        attempt: usize,
    ) -> Result<(ServiceTime, Duration), GatewayError> {
        let (now, _) = self.authorize(call, account, ServiceMethod::LlmChat, plan.digest)?;
        {
            let mut store = self.lock()?;
            Self::account(&store, account, now)?;
            self.effective_remaining(&store, scope.account)?;
            let ordinal = u8::try_from(attempt).map_err(|_| GatewayError::Durability)?;
            store.append(Event::Fence(scope, ordinal))?;
            #[cfg(test)]
            store.fail_at(FaultPoint::Fence)?;
        }
        // The durable barrier can wait. Refresh the grant and deadline again
        // after the fence has been synced and before entering the HTTP client.
        let (dispatch, _) = self.authorize(call, account, ServiceMethod::LlmChat, plan.digest)?;
        let remaining = plan
            .deadline_monotonic_ns
            .checked_sub(dispatch.monotonic_ns)
            .filter(|left| *left > 0)
            .ok_or(AuthorizationError::DeadlineExpired)?;
        Ok((dispatch, Duration::from_nanos(remaining)))
    }

    fn stop_chat(
        &self,
        scope: Scope,
        why: Stop,
        attempts: u8,
        fingerprint: Option<[u8; 32]>,
        operation: OperationId,
    ) -> Result<(), GatewayError> {
        self.lock()
            .and_then(|mut store| {
                store.append(Event::Abandon(scope, why, Some(attempts), fingerprint))
            })
            .map_err(|error| {
                if attempts == 0 {
                    error
                } else {
                    GatewayError::EffectUnknown(operation)
                }
            })
    }

    fn finish_settlement(
        &self,
        store: &mut Store,
        scope: Scope,
        plan: &Prepared,
        observed: &Observed,
    ) -> Result<(), GatewayError> {
        let actual = observed.actual.dimensions;
        if matches!(store.state.chats[&scope].phase, Phase::SettlementIntent(_)) {
            if !plan.price.covers_observation(plan.quote, observed.actual) {
                return store.append(Event::Reject(scope, Rejection::Quote));
            }
            if observed.result.is_none() {
                return store.append(Event::Reject(scope, Rejection::Oversized));
            }
            let envelope = self
                .remaining_excluding(store, scope.account, plan.quote)
                .and_then(|remaining| remaining.2.sub(actual));
            match envelope {
                Ok(_) => (),
                Err(GatewayError::Budget(_)) => {
                    return store.append(Event::Reject(scope, Rejection::Envelope));
                }
                Err(error) => {
                    store.journal.poison();
                    return Err(error);
                }
            }
            // A recovery before this fence can re-evaluate the current envelope.
            // A recovery after it must replay the exact consume, including when
            // that consume already reduced the authority's remaining balance.
            store.append(Event::AuthorizeConsume(scope))?;
            #[cfg(test)]
            store.fail_at(FaultPoint::ReadyForConsume)?;
        }
        let [
            prompt_tokens,
            completion_tokens,
            _,
            api_calls,
            cost_microcredits,
        ] = actual.0;
        let request = ConsumeLlmTokensRequest {
            account_id: scope.account,
            operation_id: plan.operation,
            idempotency_key: plan.consume_key,
            prompt_tokens,
            completion_tokens,
            reasoning_tokens: observed.actual.reasoning,
            api_calls,
            cost_microcredits,
            consumed_at_ms: plan.consumed_at_ms,
        };
        match self.resource.consume_llm_tokens(request) {
            Ok(outcome) => {
                #[cfg(test)]
                store.fail_at(FaultPoint::Consumed)?;
                store.append(Event::Settle(scope, outcome.receipt().receipt_id))?;
                #[cfg(test)]
                store.fail_at(FaultPoint::Settled)?;
                Ok(())
            }
            Err(ResourceAuthorityError::LlmTokenBudgetExhausted { .. }) => {
                store.append(Event::Reject(scope, Rejection::Envelope))
            }
            Err(_) => {
                // A failed or lost transactional response is not proof of
                // non-commit. Preserve and recover the same consume request.
                store.journal.poison();
                Err(GatewayError::Durability)
            }
        }
    }

    fn recover(
        &self,
        store: &mut Store,
        now: ServiceTime,
        starting: bool,
    ) -> Result<(), GatewayError> {
        let provisions: Vec<_> = store.state.pending_provisions.iter().copied().collect();
        for key in provisions {
            let provision = store.state.provisions[&key].0;
            match self.finish_provision(store, &provision, now) {
                Ok(_) | Err(GatewayError::Authorization(AuthorizationError::GrantExpired)) => (),
                Err(error) => return Err(error),
            }
        }
        let pending: Vec<_> = store.state.pending.iter().copied().collect();
        for scope in pending {
            if store.state.live.contains(&scope) {
                continue;
            }
            let chat = store.state.chats[&scope].clone();
            match chat.phase {
                Phase::SettlementIntent(observed) | Phase::ConsumeAuthorized(observed) => {
                    self.finish_settlement(store, scope, &chat.prepared, &observed)?;
                }
                Phase::Prepared => {
                    store.append(Event::Abandon(scope, Stop::BeforeDispatch, Some(0), None))?;
                }
                Phase::Dispatched
                    if chat.prepared.expires_wall_ms <= now.wall_ms
                        || (!starting
                            && chat.prepared.deadline_monotonic_ns <= now.monotonic_ns) =>
                {
                    store.append(Event::Abandon(scope, Stop::EffectUnknown, None, None))?;
                }
                _ => (),
            }
        }
        Ok(())
    }

    /// Reads sanitized reconciliation metadata for an already known operation.
    /// This method is for the trusted host.
    ///
    /// # Errors
    /// Fails when the journal is unavailable.
    pub fn reconciliation(
        &self,
        operation: OperationId,
    ) -> Result<Option<Reconciliation>, GatewayError> {
        let store = self.lock()?;
        let Some(scope) = store.state.operations.get(&operation) else {
            return Ok(None);
        };
        Ok(Some(recon(&store.state.chats[scope])))
    }
}

enum ChatAdmission {
    Replay(Chat),
    Ready(Prepared, ProviderRequest),
}

struct LiveCall<'a> {
    gateway: &'a Gateway,
    scope: Scope,
}

impl Drop for LiveCall<'_> {
    fn drop(&mut self) {
        if let Ok(mut store) = self.gateway.store.lock() {
            store.state.live.remove(&self.scope);
        }
    }
}

fn raw_scope(call: &CallBinding, account: &GatewayAccountBinding) -> Result<Scope, GatewayError> {
    Ok(Scope {
        account: account.account,
        raw_key: call
            .idempotency_key
            .ok_or(AuthorizationError::MissingIdempotencyKey)?,
    })
}

fn compare_digest(prior: [u8; 32], current: [u8; 32]) -> Result<(), GatewayError> {
    if prior == current {
        Ok(())
    } else {
        Err(GatewayError::Conflict)
    }
}

fn check_digest(
    method: ServiceMethod,
    payload: &[u8],
    trusted_sidecar: &[&[u8]],
    header: &[u8],
) -> Result<[u8; 32], GatewayError> {
    let mut sha = Sha256::new();
    sha.update(b"nlos/llm-gateway/request/v1");
    for part in [method_name(method).as_bytes(), payload]
        .into_iter()
        .chain(trusted_sidecar.iter().copied())
    {
        sha.update((part.len() as u64).to_be_bytes());
        sha.update(part);
    }
    let digest: [u8; 32] = sha.finalize().into();
    if !header.is_empty() && header != digest {
        return Err(AuthorizationError::BindingMismatch.into());
    }
    Ok(digest)
}

fn advisory_reply(replayed: bool, receipt: ReceiptId) -> Result<AdvisoryReply, GatewayError> {
    let result = RecordExternalUsageResult {
        schema: Some(llm_gateway_schema_identity()),
        replayed,
    };
    encode_record_external_usage_result(&result)?;
    Ok(AdvisoryReply { result, receipt })
}

fn attenuation_reply(
    provision: &Provision,
    account: GatewayAccountBinding,
) -> Result<AttenuationReply, GatewayError> {
    let result = AttenuateBudgetResult {
        schema: Some(llm_gateway_schema_identity()),
        granted_limit: Some(provision.limits.wire()),
        valid_until_ms: account.valid_until_ms(),
    };
    let receipt = ReceiptId::from_bytes(
        derive_idempotency_key(
            b"nlos/llm-gateway/child-receipt/v1",
            &[&provision.identity, account.account.as_bytes()],
        )
        .into_bytes(),
    );
    encode_attenuate_budget_result(&result)?;
    Ok(AttenuationReply {
        result,
        child: ChildBudgetBinding { account, receipt },
    })
}

fn provider_request(request: &LlmChatRequest) -> Result<ProviderRequest, GatewayError> {
    let messages = request
        .turns
        .iter()
        .map(|turn| {
            let role = match turn.role.as_str() {
                "system" => LlmChatRole::System,
                "user" => LlmChatRole::User,
                "assistant" => LlmChatRole::Assistant,
                _ => return Err(GatewayError::InvalidRequest),
            };
            Ok(LlmChatMessage {
                role,
                content: turn.content.clone(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ProviderRequest::new(messages)
        .with_max_tokens(request.max_output_tokens)
        .with_temperature(f64::from(request.temperature_milli) / 1_000.0))
}

fn observed_result(
    shim: &LlmEnforcementShim,
    plan: &Prepared,
    body: &[u8],
    fingerprint: [u8; 32],
    attempts: u8,
) -> Result<Observed, GatewayError> {
    let json = std::str::from_utf8(body).map_err(|_| GatewayError::ProviderFailure)?;
    let json = JsonValue::parse(json).map_err(|_| GatewayError::ProviderFailure)?;
    if json
        .get("model")
        .is_some_and(|model| model.as_str() != Some(plan.price.model.as_str()))
    {
        return Err(GatewayError::ProviderFailure);
    }
    let (model, content_utf8, reasoning) = extract_chat_text_fields(&json, &plan.price.model)
        .map_err(|_| GatewayError::ProviderFailure)?;
    // Provider responses can contain echoed headers. Check decoded text
    // before it can enter the durable result or be returned to a caller.
    if shim.contains_provider_credential(&content_utf8)
        || reasoning
            .as_deref()
            .is_some_and(|text| shim.contains_provider_credential(text))
    {
        return Err(GatewayError::ProviderFailure);
    }
    let actual = plan.price.actual(&json, reasoning.is_some())?;
    let result = LlmChatResult {
        schema: Some(llm_gateway_schema_identity()),
        model,
        content_utf8,
        reasoning_utf8: reasoning.unwrap_or_default(),
        usage: Some(actual.dimensions.wire()),
    };
    let result = match encode_llm_chat_result(&result) {
        Ok(encoded) => Some(Arc::from(encoded)),
        Err(CompatibilityError::FrameTooLarge { .. }) => None,
        Err(error) => return Err(error.into()),
    };
    Ok(Observed {
        actual,
        provider_fingerprint: fingerprint,
        attempts,
        result,
    })
}

fn chat_reply(chat: &Chat) -> Result<ChatReply, GatewayError> {
    let operation = chat.prepared.operation;
    match &chat.phase {
        Phase::Settled(observed, receipt) => {
            let payload = observed.result.as_deref().ok_or(GatewayError::Durability)?;
            Ok(ChatReply {
                result: decode_llm_chat_result(payload)?,
                operation,
                receipt: *receipt,
                provider_attempts: observed.attempts,
                price_version: chat.prepared.price.version,
            })
        }
        Phase::Rejected(_, Rejection::Oversized) => Err(GatewayError::FrameTooLarge),
        Phase::Rejected(..) => Err(GatewayError::SettlementRejected(operation)),
        Phase::Abandoned(reason, Some(0), _) => Err(match reason {
            Stop::BeforeDispatch | Stop::Provider | Stop::Transport => {
                GatewayError::ProviderFailure
            }
            Stop::Rights => AuthorizationError::Denied.into(),
            Stop::Deadline => AuthorizationError::DeadlineExpired.into(),
            Stop::Clock => AuthorizationError::TimeUnavailable.into(),
            _ => GatewayError::EffectUnknown(operation),
        }),
        _ => Err(GatewayError::EffectUnknown(operation)),
    }
}

fn recon(chat: &Chat) -> Reconciliation {
    let mut reconciliation = Reconciliation {
        quote: chat.prepared.quote.wire(),
        price_version: chat.prepared.price.version,
        observed: None,
        reasoning_tokens: None,
        provider_response_fingerprint: None,
        provider_attempts: None,
        durable_attempt_fences: chat.fences,
        status: ReconciliationStatus::Pending,
    };
    let observed = match &chat.phase {
        Phase::Prepared | Phase::Dispatched => None,
        Phase::SettlementIntent(observed) | Phase::ConsumeAuthorized(observed) => {
            reconciliation.status = ReconciliationStatus::Observed;
            Some(observed)
        }
        Phase::Settled(observed, receipt) => {
            reconciliation.status = ReconciliationStatus::Settled(*receipt);
            Some(observed)
        }
        Phase::Abandoned(reason, attempts, fingerprint) => {
            reconciliation.provider_attempts = *attempts;
            reconciliation.provider_response_fingerprint = *fingerprint;
            reconciliation.status = if *attempts == Some(0) && *reason != Stop::EffectUnknown {
                ReconciliationStatus::Abandoned
            } else {
                ReconciliationStatus::EffectUnknown
            };
            None
        }
        Phase::Rejected(observed, why) => {
            reconciliation.status = match why {
                Rejection::Quote => ReconciliationStatus::OverQuote,
                Rejection::Envelope => ReconciliationStatus::OverAuthorityEnvelope,
                Rejection::Oversized => ReconciliationStatus::OversizedResult,
            };
            Some(observed)
        }
    };
    if let Some(observed) = observed {
        reconciliation.observed = Some(observed.actual.dimensions.wire());
        reconciliation.reasoning_tokens = Some(observed.actual.reasoning);
        reconciliation.provider_response_fingerprint = Some(observed.provider_fingerprint);
        reconciliation.provider_attempts = Some(observed.attempts);
    }
    reconciliation
}

#[cfg(test)]
#[path = "gateway_fault_tests.rs"]
mod fault_tests;
