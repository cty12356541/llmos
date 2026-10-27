// SOTA-3 (Temporal Web UI / Perfetto UI) + SOTA-5 (Sysinternals Fault Domain):
// L3-Only Proportional TaskAttempt Race Bar & ExecutionFiber 5-Segment Waterfall Micro-Bar.
// Authority: Stage D Desktop Visual & UX Spec §2A.1 (SOTA-3/5), §3.2.D, ADR-0025 Decision 4.

import {
  createStatutoryLockBadgeElement,
  pruneVerbsAgainstStatutoryLock,
  renderStatutoryLockBadgeHtml,
  type DisclosureLevel,
  type WorkstationLocale,
} from "./status-indicator";

export type TaskAttemptOutcome =
  | "CAS_WINNER"
  | "SUPERSEDED"
  | "RUNNING"
  | "FAILED"
  | "CANCELLED";

export type AllowedAttemptVerb = "Cancel" | "Supersede" | "RequestReverify";

export interface TaskAttemptTimingInput {
  readonly attemptNumber: number;
  readonly attemptIdHex?: string;
  readonly startMs: number;
  readonly endMs: number;
  readonly outcome: TaskAttemptOutcome;
  readonly isolationDomainIdHex?: string;
  readonly retryFenceEpoch?: number;
  readonly providerIsolationUnverified?: boolean;
}

export interface ComputedAttemptSpan {
  readonly attemptNumber: number;
  readonly attemptIdHex?: string;
  readonly durationMs: number;
  readonly offsetPct: number;
  readonly widthPct: number;
  readonly outcome: TaskAttemptOutcome;
  readonly isolationDomainIdHex?: string;
  readonly retryFenceEpoch?: number;
  readonly providerIsolationUnverified: boolean;
  readonly allowedVerbs: readonly AllowedAttemptVerb[];
}

export interface AttemptRaceModel {
  readonly windowStartMs: number;
  readonly windowEndMs: number;
  readonly windowDurationMs: number;
  readonly spans: readonly ComputedAttemptSpan[];
  readonly statutoryManualCommitForbidden: true;
}

export interface AttemptRaceRenderOptions {
  readonly attempts: readonly TaskAttemptTimingInput[];
  readonly disclosure?: DisclosureLevel;
  readonly locale?: WorkstationLocale;
  readonly onAction?: (attemptNumber: number, verb: AllowedAttemptVerb) => void;
  readonly doc?: Document;
}

function sanitizeMs(ms: number): number {
  if (!Number.isFinite(ms) || ms < 0) {
    return 0;
  }
  return ms;
}

function roundPct(pct: number): number {
  const clamped = Math.min(100, Math.max(0, pct));
  return Math.round(clamped * 100) / 100;
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function computeLegalAttemptVerbs(outcome: TaskAttemptOutcome): readonly AllowedAttemptVerb[] {
  const rawCandidates =
    outcome === "RUNNING"
      ? ["Cancel", "Supersede", "RequestReverify", "ManualCommit"]
      : ["RequestReverify", "ManualCommit"];
  const pruned = pruneVerbsAgainstStatutoryLock(
    rawCandidates,
    "TASK_ATTEMPT_NO_MANUAL_COMMIT",
  );
  return pruned.filter(
    (v): v is AllowedAttemptVerb =>
      v === "Cancel" || v === "Supersede" || v === "RequestReverify",
  );
}

/** Computes wall-clock proportional race spans across sibling TaskAttempts. */
export function computeAttemptRaceModel(
  attempts: readonly TaskAttemptTimingInput[],
): AttemptRaceModel {
  if (attempts.length === 0) {
    return {
      windowStartMs: 0,
      windowEndMs: 0,
      windowDurationMs: 0,
      spans: [],
      statutoryManualCommitForbidden: true,
    };
  }

  let minStart = Number.POSITIVE_INFINITY;
  let maxEnd = 0;
  for (const item of attempts) {
    const s = sanitizeMs(item.startMs);
    const e = Math.max(s, sanitizeMs(item.endMs));
    if (s < minStart) {
      minStart = s;
    }
    if (e > maxEnd) {
      maxEnd = e;
    }
  }
  if (!Number.isFinite(minStart)) {
    minStart = 0;
  }
  const windowDurationMs = Math.max(0, maxEnd - minStart);

  const spans: ComputedAttemptSpan[] = attempts.map((item) => {
    const s = sanitizeMs(item.startMs);
    const e = Math.max(s, sanitizeMs(item.endMs));
    const durationMs = e - s;
    const offsetPct =
      windowDurationMs > 0 ? roundPct(((s - minStart) / windowDurationMs) * 100) : 0;
    const rawWidthPct =
      windowDurationMs > 0
        ? ((durationMs / windowDurationMs) * 100)
        : 100;
    const widthPct = roundPct(Math.min(100 - offsetPct, Math.max(durationMs > 0 ? 2 : 0, rawWidthPct)));

    return {
      attemptNumber: item.attemptNumber,
      attemptIdHex: item.attemptIdHex,
      durationMs,
      offsetPct,
      widthPct,
      outcome: item.outcome,
      isolationDomainIdHex: item.isolationDomainIdHex,
      retryFenceEpoch: item.retryFenceEpoch,
      providerIsolationUnverified: Boolean(item.providerIsolationUnverified),
      allowedVerbs: computeLegalAttemptVerbs(item.outcome),
    };
  });

  return {
    windowStartMs: minStart,
    windowEndMs: maxEnd,
    windowDurationMs,
    spans,
    statutoryManualCommitForbidden: true,
  };
}

function spanOutcomeModifier(outcome: TaskAttemptOutcome): string {
  switch (outcome) {
    case "SUPERSEDED":
      return " nlos-attempt-span--superseded";
    case "CANCELLED":
      return " nlos-attempt-span--cancelled";
    case "FAILED":
      return " nlos-attempt-span--failed";
    case "CAS_WINNER":
    case "RUNNING":
      return "";
  }
}

/** L3-gated HTML renderer for TaskAttempt proportional race waterfall. */
export function renderAttemptRaceHtml(options: AttemptRaceRenderOptions): string {
  const disclosure: DisclosureLevel = options.disclosure ?? "L3";
  if (disclosure !== "L3") {
    return "";
  }
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const model = computeAttemptRaceModel(options.attempts);
  const lockBadge = renderStatutoryLockBadgeHtml({
    kind: "TASK_ATTEMPT_NO_MANUAL_COMMIT",
    locale,
    disclosure: "L3",
  });

  const lanesHtml = model.spans
    .map((span) => {
      const unverifiedBadge = span.providerIsolationUnverified
        ? ` <code class="nlos-status-enum" data-badge="provider-isolation-unverified">PROVIDER_ISOLATION_UNVERIFIED</code>`
        : "";
      const epochMeta =
        span.retryFenceEpoch !== undefined ? ` fence=${span.retryFenceEpoch}` : "";
      const isoMeta = span.isolationDomainIdHex
        ? ` iso=${escapeHtml(span.isolationDomainIdHex.slice(0, 8))}`
        : "";
      return (
        `<div class="nlos-attempt-lane" data-attempt="${span.attemptNumber}" data-outcome="${span.outcome}">` +
        `<span class="nlos-mono">Attempt #${span.attemptNumber}${epochMeta}${isoMeta}</span>` +
        `<span class="nlos-waterfall-track" aria-hidden="true">` +
        `<span class="nlos-attempt-span${spanOutcomeModifier(span.outcome)}" ` +
        `style="left:${span.offsetPct}%;width:${span.widthPct}%"></span>` +
        `</span>` +
        `<span class="nlos-mono">${span.durationMs} ms ` +
        `<code class="nlos-status-enum">${escapeHtml(span.outcome)}</code>${unverifiedBadge}</span>` +
        `</div>`
      );
    })
    .join("");

  return `<div class="nlos-waterfall-group nlos-l3-only" data-component="attempt-race">${lockBadge}${lanesHtml}</div>`;
}

export function createAttemptRaceElement(
  options: AttemptRaceRenderOptions,
): HTMLElement | null {
  const disclosure: DisclosureLevel = options.disclosure ?? "L3";
  if (disclosure !== "L3") {
    return null;
  }
  const doc = options.doc ?? document;
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const model = computeAttemptRaceModel(options.attempts);

  const group = doc.createElement("div");
  group.className = "nlos-waterfall-group nlos-l3-only";
  group.setAttribute("data-component", "attempt-race");

  group.appendChild(
    createStatutoryLockBadgeElement({
      kind: "TASK_ATTEMPT_NO_MANUAL_COMMIT",
      locale,
      disclosure: "L3",
      doc,
    }),
  );

  for (const span of model.spans) {
    const lane = doc.createElement("div");
    lane.className = "nlos-attempt-lane";
    lane.setAttribute("data-attempt", String(span.attemptNumber));
    lane.setAttribute("data-outcome", span.outcome);

    const title = doc.createElement("span");
    title.className = "nlos-mono";
    const epochStr =
      span.retryFenceEpoch !== undefined ? ` fence=${span.retryFenceEpoch}` : "";
    const isoStr = span.isolationDomainIdHex
      ? ` iso=${span.isolationDomainIdHex.slice(0, 8)}`
      : "";
    title.textContent = `Attempt #${span.attemptNumber}${epochStr}${isoStr}`;

    const track = doc.createElement("span");
    track.className = "nlos-waterfall-track";
    track.setAttribute("aria-hidden", "true");

    const bar = doc.createElement("span");
    bar.className = `nlos-attempt-span${spanOutcomeModifier(span.outcome)}`;
    bar.style.left = `${span.offsetPct}%`;
    bar.style.width = `${span.widthPct}%`;
    track.appendChild(bar);

    const meta = doc.createElement("span");
    meta.className = "nlos-mono";
    meta.appendChild(doc.createTextNode(`${span.durationMs} ms `));

    const outcomeCode = doc.createElement("code");
    outcomeCode.className = "nlos-status-enum";
    outcomeCode.textContent = span.outcome;
    meta.appendChild(outcomeCode);

    if (span.providerIsolationUnverified) {
      meta.appendChild(doc.createTextNode(" "));
      const unverified = doc.createElement("code");
      unverified.className = "nlos-status-enum";
      unverified.setAttribute("data-badge", "provider-isolation-unverified");
      unverified.textContent = "PROVIDER_ISOLATION_UNVERIFIED";
      meta.appendChild(unverified);
    }

    if (options.onAction) {
      for (const verb of span.allowedVerbs) {
        meta.appendChild(doc.createTextNode(" "));
        const btn = doc.createElement("button");
        btn.type = "button";
        btn.className = "nlos-btn";
        btn.setAttribute("data-attempt-verb", verb);
        btn.textContent = verb;
        btn.addEventListener("click", () => {
          options.onAction?.(span.attemptNumber, verb);
        });
        meta.appendChild(btn);
      }
    }

    lane.appendChild(title);
    lane.appendChild(track);
    lane.appendChild(meta);
    group.appendChild(lane);
  }

  return group;
}

/* ==========================================================================
   ExecutionFiber 5-Segment Stacked Phase Waterfall Micro-Bar
   ========================================================================== */

export type FiberPhaseKey =
  | "activeCpuMs"
  | "schedulerWaitMs"
  | "externalWaitMs"
  | "backpressureWaitMs"
  | "suspendedMs";

export interface FiberWaterfallInput {
  readonly activeCpuMs: number;
  readonly schedulerWaitMs: number;
  readonly externalWaitMs: number;
  readonly backpressureWaitMs: number;
  readonly suspendedMs: number;
  readonly sharedProcessFaultDomain?: boolean;
  readonly disclosure?: DisclosureLevel;
  readonly locale?: WorkstationLocale;
  readonly doc?: Document;
}

export interface FiberPhaseSegment {
  readonly key: FiberPhaseKey;
  readonly cssModifier: "cpu" | "sched" | "ext" | "bp" | "susp";
  readonly shortLabel: string;
  readonly durationMs: number;
  readonly widthPct: number;
}

export interface FiberWaterfallModel {
  readonly totalMs: number;
  readonly segments: readonly [
    FiberPhaseSegment,
    FiberPhaseSegment,
    FiberPhaseSegment,
    FiberPhaseSegment,
    FiberPhaseSegment,
  ];
  readonly summaryText: string;
  readonly sharedProcessFaultDomain: boolean;
  readonly sharedFaultBadgeText: string | null;
}

export function computeFiberWaterfallModel(
  input: FiberWaterfallInput,
): FiberWaterfallModel {
  const cpu = sanitizeMs(input.activeCpuMs);
  const sched = sanitizeMs(input.schedulerWaitMs);
  const ext = sanitizeMs(input.externalWaitMs);
  const bp = sanitizeMs(input.backpressureWaitMs);
  const susp = sanitizeMs(input.suspendedMs);
  const totalMs = cpu + sched + ext + bp + susp;
  const locale: WorkstationLocale = input.locale ?? "zh-CN";

  const rawValues: readonly [number, number, number, number, number] = [
    cpu,
    sched,
    ext,
    bp,
    susp,
  ];

  let pcts: [number, number, number, number, number] = [0, 0, 0, 0, 0];
  if (totalMs > 0) {
    let runningSum = 0;
    let lastNonZeroIdx = -1;
    for (let i = 0; i < 5; i += 1) {
      if (rawValues[i]! > 0) {
        lastNonZeroIdx = i;
      }
    }
    for (let i = 0; i < 5; i += 1) {
      if (rawValues[i]! === 0) {
        pcts[i] = 0;
      } else if (i === lastNonZeroIdx) {
        pcts[i] = roundPct(100 - runningSum);
      } else {
        const p = roundPct((rawValues[i]! / totalMs) * 100);
        pcts[i] = p;
        runningSum += p;
      }
    }
  }

  const segments: readonly [
    FiberPhaseSegment,
    FiberPhaseSegment,
    FiberPhaseSegment,
    FiberPhaseSegment,
    FiberPhaseSegment,
  ] = [
    { key: "activeCpuMs", cssModifier: "cpu", shortLabel: "CPU", durationMs: cpu, widthPct: pcts[0] },
    { key: "schedulerWaitMs", cssModifier: "sched", shortLabel: "Sched", durationMs: sched, widthPct: pcts[1] },
    { key: "externalWaitMs", cssModifier: "ext", shortLabel: "Ext", durationMs: ext, widthPct: pcts[2] },
    { key: "backpressureWaitMs", cssModifier: "bp", shortLabel: "BP", durationMs: bp, widthPct: pcts[3] },
    { key: "suspendedMs", cssModifier: "susp", shortLabel: "Susp", durationMs: susp, widthPct: pcts[4] },
  ];

  const summaryText =
    `${totalMs} ms · CPU ${cpu} | Sched ${sched} | Ext ${ext} | BP ${bp} | Susp ${susp}`;

  const shared = Boolean(input.sharedProcessFaultDomain);
  const sharedFaultBadgeText = shared
    ? locale === "zh-CN"
      ? "共享进程域 · SHARED_THREAD_LIKE"
      : "Shared Process Domain · SHARED_THREAD_LIKE"
    : null;

  return {
    totalMs,
    segments,
    summaryText,
    sharedProcessFaultDomain: shared,
    sharedFaultBadgeText,
  };
}

export function renderFiberWaterfallHtml(input: FiberWaterfallInput): string {
  const disclosure: DisclosureLevel = input.disclosure ?? "L3";
  if (disclosure !== "L3") {
    return "";
  }
  const model = computeFiberWaterfallModel(input);
  const segHtml = model.segments
    .map(
      (s) =>
        `<span class="nlos-fiber-seg nlos-fiber-seg--${s.cssModifier}" ` +
        `data-phase="${s.key}" style="width:${s.widthPct}%"></span>`,
    )
    .join("");
  const badgeHtml =
    model.sharedFaultBadgeText !== null
      ? ` <span class="nlos-fault-domain-badge" data-fault-domain="SHARED_THREAD_LIKE">${escapeHtml(model.sharedFaultBadgeText)}</span>`
      : "";

  return (
    `<div class="nlos-waterfall-group nlos-l3-only" data-component="fiber-waterfall">` +
    `<span class="nlos-waterfall-track" role="img" aria-label="${escapeHtml(model.summaryText)}">${segHtml}</span>` +
    `<span class="nlos-mono">${escapeHtml(model.summaryText)}${badgeHtml}</span>` +
    `</div>`
  );
}

export function createFiberWaterfallElement(
  input: FiberWaterfallInput,
): HTMLElement | null {
  const disclosure: DisclosureLevel = input.disclosure ?? "L3";
  if (disclosure !== "L3") {
    return null;
  }
  const doc = input.doc ?? document;
  const model = computeFiberWaterfallModel(input);

  const root = doc.createElement("div");
  root.className = "nlos-waterfall-group nlos-l3-only";
  root.setAttribute("data-component", "fiber-waterfall");

  const track = doc.createElement("span");
  track.className = "nlos-waterfall-track";
  track.setAttribute("role", "img");
  track.setAttribute("aria-label", model.summaryText);

  for (const s of model.segments) {
    const seg = doc.createElement("span");
    seg.className = `nlos-fiber-seg nlos-fiber-seg--${s.cssModifier}`;
    seg.setAttribute("data-phase", s.key);
    seg.style.width = `${s.widthPct}%`;
    track.appendChild(seg);
  }

  const summary = doc.createElement("span");
  summary.className = "nlos-mono";
  summary.textContent = model.summaryText;

  if (model.sharedFaultBadgeText !== null) {
    summary.appendChild(doc.createTextNode(" "));
    const badge = doc.createElement("span");
    badge.className = "nlos-fault-domain-badge";
    badge.setAttribute("data-fault-domain", "SHARED_THREAD_LIKE");
    badge.textContent = model.sharedFaultBadgeText;
    summary.appendChild(badge);
  }

  root.appendChild(track);
  root.appendChild(summary);
  return root;
}
