//! In-memory projection and on-disk record codec for the gateway journal.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::num::NonZeroU64;
use std::sync::Arc;

use nlos_capability::service_call::RuntimeCaller;
use nlos_schema::{MAX_LLM_GATEWAY_PAYLOAD_BYTES, decode_llm_chat_result};
use nlos_types::{
    ApplicationId, Generation, IdempotencyKey, NamespaceId, OperationId, PrincipalId, ProcessId,
    ReceiptId, ResourceAccountId, SessionId,
};

use super::gateway_pricing::{Actual, Five, ModelAccounting, ModelPrice, valid_model};
use super::llm_gateway::GatewayError;
use super::reservation_journal::{Capacity, OVERHEAD};

pub(super) const MAX_ATTEMPTS: u8 = 8;
const SMALL_RECORD: Capacity = Capacity::new(1024, 1);
const SETTLEMENT_HOLD: Capacity = Capacity::new(2 * SMALL_RECORD.bytes, 2);
const INTENT_RECORD: Capacity = Capacity::new(MAX_LLM_GATEWAY_PAYLOAD_BYTES as u64 + 1024, 1);
pub(super) const ADVISORY_WINDOW_MS: u64 = 60_000;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct Scope {
    // An activated account is permanently and uniquely bound to its
    // principal/application/namespace/authorization domain.
    pub account: ResourceAccountId,
    pub raw_key: IdempotencyKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AccountBinding {
    pub principal: PrincipalId,
    pub application: ApplicationId,
    pub namespace: NamespaceId,
    pub domain: [u8; 16],
    pub generation: Generation,
    pub valid_until_ms: u64,
    pub parent: Option<ResourceAccountId>,
    pub recipient: Option<RuntimeCaller>,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Provision {
    pub identity: [u8; 16],
    pub caller: Option<(Scope, [u8; 32])>,
    pub binding: AccountBinding,
    pub limits: Five,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Account {
    pub binding: AccountBinding,
    pub limits: Five,
    pub settled_below: Five,
    pub pending: Five,
    pub advisory: Five,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum ProvisionStatus {
    Pending,
    Active(ResourceAccountId),
    Expired,
}

#[derive(Clone, Debug)]
pub(super) struct Prepared {
    pub digest: [u8; 32],
    pub operation: OperationId,
    pub consume_key: IdempotencyKey,
    pub quote: Five,
    pub price: ModelPrice,
    pub deadline_monotonic_ns: u64,
    pub expires_wall_ms: u64,
    pub consumed_at_ms: u64,
}

#[derive(Clone)]
pub(super) struct Observed {
    pub actual: Actual,
    pub provider_fingerprint: [u8; 32],
    pub attempts: u8,
    pub result: Option<Arc<[u8]>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stop {
    BeforeDispatch,
    Deadline,
    Rights,
    Clock,
    Transport,
    Provider,
    UsageUnknown,
    Arithmetic,
    EffectUnknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Rejection {
    Quote,
    Envelope,
    Oversized,
}

#[derive(Clone)]
pub(super) enum Phase {
    Prepared,
    Dispatched,
    SettlementIntent(Observed),
    ConsumeAuthorized(Observed),
    Settled(Observed, ReceiptId),
    Abandoned(Stop, Option<u8>, Option<[u8; 32]>),
    Rejected(Observed, Rejection),
}

#[derive(Clone)]
pub(super) struct Chat {
    pub prepared: Prepared,
    pub fences: u8,
    pub phase: Phase,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Advisory {
    pub key: Scope,
    pub digest: [u8; 32],
    pub session: SessionId,
    pub observed_wall_ms: u64,
    pub usage: Five,
    pub receipt: ReceiptId,
}

pub(super) enum Event {
    Provision(Provision),
    Activate([u8; 16], ResourceAccountId),
    ExpireProvision([u8; 16]),
    Prepare(Scope, Prepared),
    Fence(Scope, u8),
    Observe(Scope, Observed),
    AuthorizeConsume(Scope),
    Settle(Scope, ReceiptId),
    Abandon(Scope, Stop, Option<u8>, Option<[u8; 32]>),
    Reject(Scope, Rejection),
    Advisory(Advisory),
}

#[derive(Default)]
pub(super) struct State {
    pub accounts: BTreeMap<ResourceAccountId, Account>,
    pub provisions: BTreeMap<[u8; 16], (Provision, ProvisionStatus)>,
    pub child_requests: BTreeMap<Scope, [u8; 16]>,
    pub pending_provisions: BTreeSet<[u8; 16]>,
    pub chats: BTreeMap<Scope, Chat>,
    pub pending: BTreeSet<Scope>,
    pub operations: BTreeMap<OperationId, Scope>,
    pub live: BTreeSet<Scope>,
    pub advisories: BTreeMap<Scope, Advisory>,
    pub advisory_high_water: u64,
    pub wall_floor: u64,
    advisory_recent: VecDeque<(u64, PrincipalId, SessionId)>,
    session_rows: BTreeMap<(PrincipalId, SessionId), usize>,
    principal_rows: BTreeMap<PrincipalId, usize>,
    pub reserved: Capacity,
}

impl State {
    pub fn chain(
        &self,
        account: ResourceAccountId,
    ) -> Result<Vec<ResourceAccountId>, GatewayError> {
        let mut chain = Vec::new();
        let mut current = Some(account);
        while let Some(id) = current {
            let entry = self.accounts.get(&id).ok_or(GatewayError::Durability)?;
            chain.push(id);
            current = entry.binding.parent;
        }
        Ok(chain)
    }

    pub fn account_is_active(&self, account: ResourceAccountId, now_ms: u64) -> bool {
        self.chain(account).is_ok_and(|chain| {
            chain
                .iter()
                .all(|id| self.accounts[id].binding.valid_until_ms >= now_ms)
        })
    }

    pub fn prune_advisory(&mut self, now_ms: u64) {
        while let Some(&(time, principal, session)) = self.advisory_recent.front() {
            if now_ms.saturating_sub(time) < ADVISORY_WINDOW_MS {
                break;
            }
            self.advisory_recent.pop_front();
            decrement(&mut self.session_rows, &(principal, session));
            decrement(&mut self.principal_rows, &principal);
        }
    }

    pub fn advisory_quota(&self, caller: &RuntimeCaller, cap: usize) -> bool {
        self.principal_rows
            .get(&caller.principal)
            .copied()
            .unwrap_or(0)
            < cap
            && self
                .session_rows
                .get(&(caller.principal, caller.session))
                .copied()
                .unwrap_or(0)
                < cap
    }

    pub fn next_hold(&self, event: &Event) -> Result<Capacity, GatewayError> {
        let (old, new) = match event {
            Event::Prepare(..) => (Capacity::ZERO, chat_hold(0)),
            Event::Fence(_, ordinal) if *ordinal > 0 && *ordinal <= MAX_ATTEMPTS => {
                (chat_hold(ordinal - 1), chat_hold(*ordinal))
            }
            Event::Observe(key, _) => (chat_hold(self.chat(key)?.fences), SETTLEMENT_HOLD),
            Event::AuthorizeConsume(_) => (SETTLEMENT_HOLD, SMALL_RECORD),
            Event::Abandon(key, ..) => (chat_hold(self.chat(key)?.fences), Capacity::ZERO),
            Event::Reject(key, _) => (
                if matches!(self.chat(key)?.phase, Phase::ConsumeAuthorized(_)) {
                    SMALL_RECORD
                } else {
                    SETTLEMENT_HOLD
                },
                Capacity::ZERO,
            ),
            Event::Settle(..) | Event::Activate(..) | Event::ExpireProvision(..) => {
                (SMALL_RECORD, Capacity::ZERO)
            }
            Event::Provision(..) => (Capacity::ZERO, SMALL_RECORD),
            Event::Advisory(..) => (Capacity::ZERO, Capacity::ZERO),
            Event::Fence(..) => return Err(GatewayError::Durability),
        };
        self.reserved.sub(old)?.add(new)
    }

    // Each branch is one replay transition and validates its ordering here.
    #[allow(clippy::too_many_lines)]
    pub fn apply(&mut self, event: Event) -> Result<(), GatewayError> {
        let next_hold = self.next_hold(&event)?;
        match event {
            Event::Provision(provision) => {
                if self.provisions.contains_key(&provision.identity)
                    || !provision.limits.authority_sized()
                    || provision.binding.valid_until_ms < provision.created_at_ms
                {
                    return Err(GatewayError::Durability);
                }
                if let Some((scope, _)) = provision.caller {
                    if provision.binding.parent != Some(scope.account)
                        || !self.accounts.contains_key(&scope.account)
                        || self
                            .child_requests
                            .insert(scope, provision.identity)
                            .is_some()
                    {
                        return Err(GatewayError::Durability);
                    }
                } else if provision.binding.parent.is_some() {
                    return Err(GatewayError::Durability);
                }
                self.wall_floor = self.wall_floor.max(provision.created_at_ms);
                self.pending_provisions.insert(provision.identity);
                self.provisions
                    .insert(provision.identity, (provision, ProvisionStatus::Pending));
            }
            Event::Activate(identity, account) => {
                let (provision, status) = self
                    .provisions
                    .get_mut(&identity)
                    .ok_or(GatewayError::Durability)?;
                if !self.pending_provisions.remove(&identity)
                    || self.accounts.contains_key(&account)
                {
                    return Err(GatewayError::Durability);
                }
                *status = ProvisionStatus::Active(account);
                self.accounts.insert(
                    account,
                    Account {
                        binding: provision.binding,
                        limits: provision.limits,
                        settled_below: Five::default(),
                        pending: Five::default(),
                        advisory: Five::default(),
                    },
                );
            }
            Event::ExpireProvision(identity) => {
                let (_, status) = self
                    .provisions
                    .get_mut(&identity)
                    .ok_or(GatewayError::Durability)?;
                if !self.pending_provisions.remove(&identity) {
                    return Err(GatewayError::Durability);
                }
                *status = ProvisionStatus::Expired;
            }
            Event::Prepare(scope, plan) => {
                if self.chats.contains_key(&scope)
                    || self.operations.insert(plan.operation, scope).is_some()
                    || !plan.quote.authority_sized()
                    || plan.quote.0[3] != 1
                    || plan.deadline_monotonic_ns == 0
                    || plan.expires_wall_ms < plan.consumed_at_ms
                {
                    return Err(GatewayError::Durability);
                }
                for account in self.chain(scope.account)? {
                    let account = self
                        .accounts
                        .get_mut(&account)
                        .ok_or(GatewayError::Durability)?;
                    account.pending = account.pending.add(plan.quote)?;
                }
                self.wall_floor = self.wall_floor.max(plan.consumed_at_ms);
                self.pending.insert(scope);
                self.chats.insert(
                    scope,
                    Chat {
                        prepared: plan,
                        fences: 0,
                        phase: Phase::Prepared,
                    },
                );
            }
            Event::Fence(scope, ordinal) => {
                let chat = self.chat_mut(&scope)?;
                if chat.fences + 1 != ordinal
                    || !matches!(chat.phase, Phase::Prepared | Phase::Dispatched)
                {
                    return Err(GatewayError::Durability);
                }
                chat.fences = ordinal;
                chat.phase = Phase::Dispatched;
            }
            Event::Observe(scope, observed) => {
                let chat = self.chat_mut(&scope)?;
                if !matches!(chat.phase, Phase::Dispatched)
                    || observed.attempts != chat.fences
                    || observed.actual.dimensions.0[3] != 1
                    || observed.actual.dimensions.0[0]
                        .checked_add(observed.actual.dimensions.0[1])
                        .and_then(|sum| sum.checked_add(observed.actual.reasoning))
                        != Some(observed.actual.dimensions.0[2])
                {
                    return Err(GatewayError::Durability);
                }
                if let Some(payload) = observed.result.as_deref() {
                    let result =
                        decode_llm_chat_result(payload).map_err(|_| GatewayError::Durability)?;
                    if result.usage != Some(observed.actual.dimensions.wire())
                        || result.model != chat.prepared.price.model
                    {
                        return Err(GatewayError::Durability);
                    }
                }
                chat.phase = Phase::SettlementIntent(observed);
            }
            Event::AuthorizeConsume(scope) => {
                let chat = self.chat_mut(&scope)?;
                let Phase::SettlementIntent(observed) = &chat.phase else {
                    return Err(GatewayError::Durability);
                };
                chat.phase = Phase::ConsumeAuthorized(observed.clone());
            }
            Event::Settle(scope, receipt) => {
                let Phase::ConsumeAuthorized(observed) = &self.chat(&scope)?.phase else {
                    return Err(GatewayError::Durability);
                };
                let observed = observed.clone();
                if observed.result.is_none() {
                    return Err(GatewayError::Durability);
                }
                let actual = observed.actual.dimensions;
                self.finish(scope, Phase::Settled(observed, receipt), Some(actual))?;
            }
            Event::Reject(scope, rejection) => {
                let observed = self.observed(&scope)?;
                self.finish(scope, Phase::Rejected(observed, rejection), None)?;
            }
            Event::Abandon(scope, stop, attempts, fingerprint) => {
                let chat = self.chat(&scope)?;
                if !matches!(chat.phase, Phase::Prepared | Phase::Dispatched)
                    || attempts.is_some_and(|count| count > chat.fences)
                {
                    return Err(GatewayError::Durability);
                }
                self.finish(scope, Phase::Abandoned(stop, attempts, fingerprint), None)?;
            }
            Event::Advisory(row) => {
                if row.observed_wall_ms < self.advisory_high_water
                    || self.advisories.contains_key(&row.key)
                {
                    return Err(GatewayError::Durability);
                }
                let account = self
                    .accounts
                    .get_mut(&row.key.account)
                    .ok_or(GatewayError::Durability)?;
                account.advisory = account.advisory.add(row.usage)?;
                let principal = account.binding.principal;
                self.prune_advisory(row.observed_wall_ms);
                self.advisory_high_water = row.observed_wall_ms;
                self.wall_floor = self.wall_floor.max(row.observed_wall_ms);
                self.advisory_recent
                    .push_back((row.observed_wall_ms, principal, row.session));
                *self
                    .session_rows
                    .entry((principal, row.session))
                    .or_default() += 1;
                *self.principal_rows.entry(principal).or_default() += 1;
                self.advisories.insert(row.key, row);
            }
        }
        self.reserved = next_hold;
        Ok(())
    }

    fn chat(&self, key: &Scope) -> Result<&Chat, GatewayError> {
        self.chats.get(key).ok_or(GatewayError::Durability)
    }

    fn chat_mut(&mut self, key: &Scope) -> Result<&mut Chat, GatewayError> {
        self.chats.get_mut(key).ok_or(GatewayError::Durability)
    }

    fn observed(&self, key: &Scope) -> Result<Observed, GatewayError> {
        match &self.chat(key)?.phase {
            Phase::SettlementIntent(observed) | Phase::ConsumeAuthorized(observed) => {
                Ok(observed.clone())
            }
            _ => Err(GatewayError::Durability),
        }
    }

    fn finish(
        &mut self,
        key: Scope,
        phase: Phase,
        actual: Option<Five>,
    ) -> Result<(), GatewayError> {
        let quote = self.chat(&key)?.prepared.quote;
        for id in self.chain(key.account)? {
            let account = self.accounts.get_mut(&id).ok_or(GatewayError::Durability)?;
            account.pending = account.pending.sub(quote)?;
            if let Some(actual) = actual.filter(|_| id != key.account) {
                account.settled_below = account.settled_below.add(actual)?;
            }
        }
        self.chat_mut(&key)?.phase = phase;
        self.pending.remove(&key);
        Ok(())
    }
}

fn decrement<K: Ord>(counts: &mut BTreeMap<K, usize>, key: &K) {
    if let Some(count) = counts.get_mut(key) {
        *count -= 1;
        if *count == 0 {
            counts.remove(key);
        }
    }
}

const fn chat_hold(fences: u8) -> Capacity {
    let small = u64::from_le_bytes([MAX_ATTEMPTS - fences + 2, 0, 0, 0, 0, 0, 0, 0]);
    Capacity::new(
        small * SMALL_RECORD.bytes + INTENT_RECORD.bytes,
        small + INTENT_RECORD.rows,
    )
}

impl Event {
    #[allow(clippy::too_many_lines)]
    pub fn encode(&self) -> Result<Vec<u8>, GatewayError> {
        let mut out = WriteRecord(Vec::new());
        match self {
            Self::Provision(provision) => {
                out.tag(1);
                out.bytes(&provision.identity);
                if let Some((scope, digest)) = provision.caller {
                    out.tag(1);
                    out.scope(scope);
                    out.bytes(&digest);
                } else {
                    out.tag(0);
                }
                out.account(provision.binding);
                out.five(provision.limits);
                out.u64(provision.created_at_ms);
            }
            Self::Activate(identity, account) => {
                out.tag(2);
                out.bytes(identity);
                out.bytes(account.as_bytes());
            }
            Self::ExpireProvision(identity) => {
                out.tag(3);
                out.bytes(identity);
            }
            Self::Prepare(scope, prepared) => {
                out.tag(4);
                out.scope(*scope);
                out.bytes(&prepared.digest);
                out.bytes(prepared.operation.as_bytes());
                out.bytes(prepared.consume_key.as_bytes());
                out.five(prepared.quote);
                out.sized(prepared.price.model.as_bytes())?;
                out.u64(prepared.price.version);
                out.u32(prepared.price.max_output_tokens);
                match prepared.price.accounting {
                    ModelAccounting::NoReasoning => out.tag(0),
                    ModelAccounting::InclusiveReasoning => out.tag(1),
                    ModelAccounting::SeparateReasoning {
                        quote_reasoning_tokens,
                    } => {
                        out.tag(2);
                        out.u32(quote_reasoning_tokens);
                    }
                }
                out.u64(prepared.price.prompt_microcredits_per_kilotoken);
                out.u64(prepared.price.output_microcredits_per_kilotoken);
                out.u64(prepared.deadline_monotonic_ns);
                out.u64(prepared.expires_wall_ms);
                out.u64(prepared.consumed_at_ms);
            }
            Self::Fence(scope, ordinal) => {
                out.tag(5);
                out.scope(*scope);
                out.tag(*ordinal);
            }
            Self::Observe(scope, observed) => {
                out.tag(6);
                out.scope(*scope);
                out.five(observed.actual.dimensions);
                out.u64(observed.actual.reasoning);
                out.bytes(&observed.provider_fingerprint);
                out.tag(observed.attempts);
                if let Some(result) = &observed.result {
                    out.tag(1);
                    out.sized(result)?;
                } else {
                    out.tag(0);
                }
            }
            Self::AuthorizeConsume(scope) => {
                out.tag(11);
                out.scope(*scope);
            }
            Self::Settle(scope, receipt) => {
                out.tag(7);
                out.scope(*scope);
                out.bytes(receipt.as_bytes());
            }
            Self::Abandon(scope, stop, attempts, fingerprint) => {
                out.tag(8);
                out.scope(*scope);
                out.tag(stop.code());
                out.tag(attempts.unwrap_or(u8::MAX));
                if let Some(fingerprint) = fingerprint {
                    out.tag(1);
                    out.bytes(fingerprint);
                } else {
                    out.tag(0);
                }
            }
            Self::Reject(scope, rejection) => {
                out.tag(9);
                out.scope(*scope);
                out.tag(match rejection {
                    Rejection::Quote => 0,
                    Rejection::Envelope => 1,
                    Rejection::Oversized => 2,
                });
            }
            Self::Advisory(advisory) => {
                out.tag(10);
                out.scope(advisory.key);
                out.bytes(&advisory.digest);
                out.bytes(advisory.session.as_bytes());
                out.u64(advisory.observed_wall_ms);
                out.five(advisory.usage);
                out.bytes(advisory.receipt.as_bytes());
            }
        }
        // Include framing in the per-state reservation bound.
        let allowed = if matches!(self, Self::Observe(..)) {
            INTENT_RECORD
        } else {
            SMALL_RECORD
        };
        if (out.0.len() + OVERHEAD) as u64 > allowed.bytes {
            return Err(GatewayError::Durability);
        }
        Ok(out.0)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, GatewayError> {
        let mut input = ReadRecord(bytes);
        let event = match input.tag()? {
            1 => Self::Provision(Provision {
                identity: input.array()?,
                caller: if input.present()? {
                    Some((input.scope()?, input.array()?))
                } else {
                    None
                },
                binding: input.account()?,
                limits: input.five()?,
                created_at_ms: input.u64()?,
            }),
            2 => Self::Activate(
                input.array()?,
                ResourceAccountId::from_bytes(input.array()?),
            ),
            3 => Self::ExpireProvision(input.array()?),
            4 => Self::Prepare(input.scope()?, input.prepare()?),
            5 => Self::Fence(input.scope()?, input.tag()?),
            6 => Self::Observe(input.scope()?, input.observed()?),
            7 => Self::Settle(input.scope()?, ReceiptId::from_bytes(input.array()?)),
            8 => {
                let scope = input.scope()?;
                let stop = Stop::from_code(input.tag()?)?;
                let attempts = match input.tag()? {
                    u8::MAX => None,
                    count if count <= MAX_ATTEMPTS => Some(count),
                    _ => return Err(GatewayError::Durability),
                };
                let fingerprint = if input.present()? {
                    Some(input.array()?)
                } else {
                    None
                };
                Self::Abandon(scope, stop, attempts, fingerprint)
            }
            9 => Self::Reject(
                input.scope()?,
                match input.tag()? {
                    0 => Rejection::Quote,
                    1 => Rejection::Envelope,
                    2 => Rejection::Oversized,
                    _ => return Err(GatewayError::Durability),
                },
            ),
            11 => Self::AuthorizeConsume(input.scope()?),
            10 => Self::Advisory(Advisory {
                key: input.scope()?,
                digest: input.array()?,
                session: SessionId::from_bytes(input.array()?),
                observed_wall_ms: input.u64()?,
                usage: input.five()?,
                receipt: ReceiptId::from_bytes(input.array()?),
            }),
            _ => return Err(GatewayError::Durability),
        };
        if !input.0.is_empty() {
            return Err(GatewayError::Durability);
        }
        Ok(event)
    }
}

impl Stop {
    const fn code(self) -> u8 {
        match self {
            Self::BeforeDispatch => 0,
            Self::Deadline => 1,
            Self::Rights => 2,
            Self::Clock => 3,
            Self::Transport => 4,
            Self::Provider => 5,
            Self::UsageUnknown => 6,
            Self::Arithmetic => 7,
            Self::EffectUnknown => 8,
        }
    }

    const fn from_code(code: u8) -> Result<Self, GatewayError> {
        Ok(match code {
            0 => Self::BeforeDispatch,
            1 => Self::Deadline,
            2 => Self::Rights,
            3 => Self::Clock,
            4 => Self::Transport,
            5 => Self::Provider,
            6 => Self::UsageUnknown,
            7 => Self::Arithmetic,
            8 => Self::EffectUnknown,
            _ => return Err(GatewayError::Durability),
        })
    }
}

struct WriteRecord(Vec<u8>);

impl WriteRecord {
    fn tag(&mut self, value: u8) {
        self.0.push(value);
    }
    fn bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }
    fn u64(&mut self, value: u64) {
        self.bytes(&value.to_be_bytes());
    }
    fn u32(&mut self, value: u32) {
        self.bytes(&value.to_be_bytes());
    }
    fn five(&mut self, five: Five) {
        for amount in five.0 {
            self.u64(amount);
        }
    }
    fn sized(&mut self, bytes: &[u8]) -> Result<(), GatewayError> {
        self.u32(u32::try_from(bytes.len()).map_err(|_| GatewayError::Durability)?);
        self.bytes(bytes);
        Ok(())
    }
    fn scope(&mut self, scope: Scope) {
        self.bytes(scope.account.as_bytes());
        self.bytes(scope.raw_key.as_bytes());
    }
    fn account(&mut self, binding: AccountBinding) {
        self.bytes(binding.principal.as_bytes());
        self.bytes(binding.application.as_bytes());
        self.bytes(binding.namespace.as_bytes());
        self.bytes(&binding.domain);
        self.u64(binding.generation.get());
        self.u64(binding.valid_until_ms);
        if let Some(parent) = binding.parent {
            self.tag(1);
            self.bytes(parent.as_bytes());
        } else {
            self.tag(0);
        }
        if let Some(recipient) = binding.recipient {
            self.tag(1);
            self.bytes(recipient.process.as_bytes());
            self.u64(recipient.process_generation.get());
            self.bytes(recipient.session.as_bytes());
        } else {
            self.tag(0);
        }
    }
}

struct ReadRecord<'a>(&'a [u8]);

impl<'a> ReadRecord<'a> {
    fn read(&mut self, length: usize) -> Result<&'a [u8], GatewayError> {
        let (head, tail) = self
            .0
            .split_at_checked(length)
            .ok_or(GatewayError::Durability)?;
        self.0 = tail;
        Ok(head)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N], GatewayError> {
        self.read(N)?
            .try_into()
            .map_err(|_| GatewayError::Durability)
    }
    fn tag(&mut self) -> Result<u8, GatewayError> {
        Ok(self.array::<1>()?[0])
    }
    fn present(&mut self) -> Result<bool, GatewayError> {
        match self.tag()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(GatewayError::Durability),
        }
    }
    fn u64(&mut self) -> Result<u64, GatewayError> {
        Ok(u64::from_be_bytes(self.array()?))
    }
    fn u32(&mut self) -> Result<u32, GatewayError> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    fn generation(&mut self) -> Result<Generation, GatewayError> {
        NonZeroU64::new(self.u64()?)
            .map(Generation::new)
            .ok_or(GatewayError::Durability)
    }
    fn five(&mut self) -> Result<Five, GatewayError> {
        let mut values = [0; 5];
        for value in &mut values {
            *value = self.u64()?;
        }
        Ok(Five(values))
    }
    fn sized(&mut self, max: usize) -> Result<&'a [u8], GatewayError> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(GatewayError::Durability);
        }
        self.read(len)
    }
    fn scope(&mut self) -> Result<Scope, GatewayError> {
        Ok(Scope {
            account: ResourceAccountId::from_bytes(self.array()?),
            raw_key: IdempotencyKey::from_bytes(self.array()?),
        })
    }
    fn account(&mut self) -> Result<AccountBinding, GatewayError> {
        let principal = PrincipalId::from_bytes(self.array()?);
        let application = ApplicationId::from_bytes(self.array()?);
        let namespace = NamespaceId::from_bytes(self.array()?);
        let domain = self.array()?;
        let generation = self.generation()?;
        let valid_until_ms = self.u64()?;
        let parent = if self.present()? {
            Some(ResourceAccountId::from_bytes(self.array()?))
        } else {
            None
        };
        let recipient = if self.present()? {
            Some(RuntimeCaller {
                principal,
                application,
                process: ProcessId::from_bytes(self.array()?),
                process_generation: self.generation()?,
                session: SessionId::from_bytes(self.array()?),
            })
        } else {
            None
        };
        if parent.is_some() != recipient.is_some() {
            return Err(GatewayError::Durability);
        }
        Ok(AccountBinding {
            principal,
            application,
            namespace,
            domain,
            generation,
            valid_until_ms,
            parent,
            recipient,
        })
    }
    fn prepare(&mut self) -> Result<Prepared, GatewayError> {
        let digest = self.array()?;
        let operation = OperationId::from_bytes(self.array()?);
        let consume_key = IdempotencyKey::from_bytes(self.array()?);
        let quote = self.five()?;
        let model = std::str::from_utf8(self.sized(super::gateway_pricing::MAX_MODEL_NAME_BYTES)?)
            .map_err(|_| GatewayError::Durability)?
            .to_owned();
        let version = self.u64()?;
        let max_output_tokens = self.u32()?;
        let accounting = match self.tag()? {
            0 => ModelAccounting::NoReasoning,
            1 => ModelAccounting::InclusiveReasoning,
            2 => ModelAccounting::SeparateReasoning {
                quote_reasoning_tokens: self.u32()?,
            },
            _ => return Err(GatewayError::Durability),
        };
        let price = ModelPrice {
            model,
            version,
            max_output_tokens,
            accounting,
            prompt_microcredits_per_kilotoken: self.u64()?,
            output_microcredits_per_kilotoken: self.u64()?,
        };
        if !valid_model(&price.model) || !price.valid() {
            return Err(GatewayError::Durability);
        }
        Ok(Prepared {
            digest,
            operation,
            consume_key,
            quote,
            price,
            deadline_monotonic_ns: self.u64()?,
            expires_wall_ms: self.u64()?,
            consumed_at_ms: self.u64()?,
        })
    }
    fn observed(&mut self) -> Result<Observed, GatewayError> {
        let dimensions = self.five()?;
        let reasoning = self.u64()?;
        let provider_fingerprint = self.array()?;
        let attempts = self.tag()?;
        let result = if self.present()? {
            Some(Arc::from(self.sized(MAX_LLM_GATEWAY_PAYLOAD_BYTES)?))
        } else {
            None
        };
        Ok(Observed {
            actual: Actual {
                dimensions,
                reasoning,
            },
            provider_fingerprint,
            attempts,
            result,
        })
    }
}
