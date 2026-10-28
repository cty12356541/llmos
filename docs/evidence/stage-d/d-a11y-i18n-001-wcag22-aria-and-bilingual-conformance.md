# D-CONTROL-SETTINGS-A11Y Slice 2 Evidence: WCAG 2.2 AA Contrast, ARIA Semantic Tree, Keyboard Navigation, Reduced Motion & Versioned `zh-CN / en-US` Localization (`d-a11y-i18n-001`)

- **Evidence ID**: `D-A11Y-I18N-001`
- **Wave / Task / Attempt**: `W73` / `W73-D-A11Y-I18N` (`D-CONTROL-SETTINGS-A11Y` Slice 2, Attempt 1)
- **Verification Status**: `PARTIAL_PASS`
- **Evidence Level**: `H4` (Authenticated / Deterministic Tauri Backend & Workstation Frontend Integration Suite)
- **Base Commit**: `5c052e53295972c2e261b0bd50de26628edfdfdc`
- **Road / ADR / Spec Authorities**:
  - `docs/design/06-架构设计总纲-v0.5.md` (`[DUI-WINDOW-001]`, `[TUI-CONSENT-001]`, `[TUI-EXT-001]`, `[ROAD-D-001]`, `[UI-PARITY-001]`, `[CONS-CTRL-001]`, `[RCPT-UNDO-001]`)
  - `docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md` (Decisions 1–4, Gates `D-GATE-001..006`)
  - `docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md` (`§0.1 ANTI-AI-01..07`, `§1.1–§1.3`, `§3.4`, `§3.7 #4`, `§4.1`, `§4.3`)
  - `docs/management/stage-d-progress.md` (`§D.3.1 #8 D-CONTROL-SETTINGS-A11Y`, `§D.5.2 W73-D-A11Y-I18N`)

---

## 1. Delivered Write-Set (`WriteSet(W73-D-A11Y-I18N)`)

### 1.1 Versioned Bilingual Localization Engine (`desktop/src/i18n/**`, `[TUI-EXT-001]`)

1. [zh-CN.ts](../../../desktop/src/i18n/locales/zh-CN.ts) & [en-US.ts](../../../desktop/src/i18n/locales/en-US.ts)
   - Defines versioned (`bundleVersion: "1.0.0-stage-d"`, `schemaRevision: 1`) strongly-typed translation dictionaries (`LocaleResourceBundle`) covering all `Z0–Z4` planes, all 16 canonical `Task` lifecycle states (`PROPOSED` through `CANCELLED`) plus 8 extended workstation states (24 total), `L2` attribution drawer & 6 statutory non-tamperable lock badges, all 12 `SystemControlCenter` domains, `Z4` UAC/Polkit security consent & chooser/taint prompts, notifications & one-time `ActionToken` states, Spotlight (`Ctrl+K`) read-only candidate search, offline autonomy & connectivity posture, screen-reader `aria-live` announcements, and `L3`-gated forensic/DAG diagnostics.
   - Enforces `ANTI-AI-05` (`0` pictographic emojis across all keys) and `ANTI-AI-07` (`0` internal kernel jargon tokens such as `Z0..Z4`, `TypedIntent`, `0x1F`, `Quota`, `TaskAttempt`, `Fiber`, `Cell`, `receipt_hex`, `control_command_id` across all `L0/L1/L2` user-facing keys in both `zh-CN` and `en-US`).
2. [index.ts](../../../desktop/src/i18n/index.ts)
   - Implements `LocalizationController` supporting atomic runtime locale switching (`zh-CN <-> en-US`) with monotonic `switchGeneration` tracking, listener subscriptions, fallback-free key lookup (`t(keyPath, params)`), `Intl.NumberFormat` tabular number formatting (`formatTabularNumber`), fallback-free key parity auditing (`verifyLocaleKeyParity()`), `L0/L1/L2` zero-jargon auditing (`verifyLocaleZeroJargon()`), and zero-emoji auditing (`verifyLocaleZeroEmoji()`).

### 1.2 Workstation Accessibility Engine (`desktop/src/a11y/**`, `[DUI-WINDOW-001]`, `WCAG 2.2 AA`)

1. [focus-manager.ts](../../../desktop/src/a11y/focus-manager.ts)
   - Implements `WorkstationFocusManager` providing linear `Tab` / `Shift+Tab` traversal across active `Z1–Z3` planes (skipping decorative `Z0`), roving tabindex (`tabindex="0"` active, `tabindex="-1"` inactive) with `ArrowUp / ArrowDown / ArrowLeft / ArrowRight / Home / End` and treegrid expand/collapse for `Task Space`, the 10-layer `SystemControlCenter` ownership tree, and `TaskPlan` DAG nodes, plus deterministic `Escape` dismissal stack with originating trigger focus restoration.
   - Implements global workstation keyboard shortcuts: `Ctrl+Space` (`TOGGLE_OMNIBAR_PALETTE`), `Ctrl+K` (`TOGGLE_SPOTLIGHT_SEARCH`), and `Ctrl+Shift+E` (`TOGGLE_L3_DIAGNOSTIC`).
   - Enforces the `Z4` Modal Focus Trap (`[TUI-CONSENT-001]`, `verifyZ4ModalFocusTrap`): while `Z4` is active, `Tab` and `Shift+Tab` wrap strictly within enabled `Z4` controls, programmatic focus requests targeting `Z0–Z3` and `Ctrl+Space` / `Ctrl+K` shortcuts are blocked and counted (`trapEscapeAttemptCount`), and `Escape` restores focus to the originating `Z1–Z3` trigger.
2. [aria-tree-auditor.ts](../../../desktop/src/a11y/aria-tree-auditor.ts)
   - Implements `buildCanonicalWorkstationAriaTree`, `auditWorkstationAriaTree`, and `WorkstationLiveRegionAnnouncer` covering all `Z0–Z4` landmarks (`main`, `region`, `complementary`, `contentinfo`, `grid`, `treegrid`, `dialog`, `alertdialog`, `status`, `progressbar`, `toolbar`, `button`), verifying non-empty jargon-free accessible names, `>= 26x26px` minimum interactive hit targets (`WCAG 2.2 AA 2.5.8`), keyboard drag-equivalent menus (`WCAG 2.2 AA 2.5.7`), automatic `aria-hidden="true"` background shielding on `Z1–Z3` when `Z4` (`role="alertdialog"`, `aria-modal="true"`) is active, and dual-channel `aria-live` (`polite` for `L1` task state transitions, `assertive` for `Z4` security prompts and `QUARANTINED` isolation alerts).
3. [contrast-and-motion.ts](../../../desktop/src/a11y/contrast-and-motion.ts) & [a11y.css](../../../desktop/src/a11y/a11y.css)
   - Implements `computeRelativeLuminance`, `computeContrastRatio`, and `auditWorkstationContrast` across all 20 canonical token pairs in `Graphite Workstation` (dark) and `Paper Workstation` (light), verifying `>= 4.5:1` for normal text (observed min `4.83:1`, primary text `14.72:1` / `17.74:1`) and `>= 3.0:1` for `2px` focus rings (`--ws-accent` `#3B82F6` / `#1D4ED8`, observed min `4.08:1`).
   - Implements `ReducedMotionController` and `@media (prefers-reduced-motion: reduce)` / `[data-nlos-reduced-motion="true"]` rules freezing the `32x20` Gerono verification seal to a static closed curve (`static-reduced-motion`, `0` active RAF loops) and collapsing `--nlos-dur-fast` / `--nlos-dur-base` transition durations to `0ms`.
   - Enforces `ANTI-AI-01..07` in `a11y.css`: `100%` solid opaque tokens, `0` `backdrop-filter`, `0` neon glow, and `border-radius` strictly in `{0px, 2px, 4px, 6px}`.
4. [index.ts](../../../desktop/src/a11y/index.ts) & [a11y-i18n.test.ts](../../../desktop/src/a11y/__tests__/a11y-i18n.test.ts)
   - Unified `WorkstationA11yI18nSuiteController` facade and 7-suite (`148` assertions) deterministic TypeScript verification harness.

### 1.3 Rust Tauri Backend Conformance Module (`desktop/src-tauri/**`)

1. [a11y_i18n.rs](../../../desktop/src-tauri/src/a11y_i18n.rs)
   - Implements `A11yI18nState` and 6 `#[tauri::command]` handlers with `pub fn ..._inner(...)` test-callable entrypoints:
     - `a11y_get_locale_bundle` / `a11y_get_locale_bundle_inner`
     - `a11y_switch_locale` / `a11y_switch_locale_inner`
     - `a11y_audit_wcag_contrast` / `a11y_audit_wcag_contrast_inner`
     - `a11y_audit_aria_landmarks` / `a11y_audit_aria_landmarks_inner`
     - `a11y_verify_keyboard_focus_trap` / `a11y_verify_keyboard_focus_trap_inner`
     - `a11y_set_reduced_motion_override` / `a11y_set_reduced_motion_override_inner`
2. [a11y_i18n_conformance_side.rs](../../../desktop/src-tauri/tests/a11y_i18n_conformance_side.rs)
   - End-to-end Rust integration & negative-bypass test suite covering bilingual resource parity, runtime locale switching, `L0/L1/L2` zero-jargon and zero-emoji guards, 20-pair `WCAG 2.2 AA` luminance contrast ratios, `Z0–Z4` ARIA landmark & `26px` hit-target auditing, `Z4` modal focus-trap isolation, and `prefers-reduced-motion` Gerono seal & transition freeze.

---

## 2. Verification Receipts

1. **TypeScript Typecheck (`npx tsc --noEmit`)**:
   - Command: `npx tsc --noEmit` (in `d:\llmos\desktop`)
   - Result: **0 errors, 0 warnings** (exit code `0`).
2. **Frontend Accessibility & Localization Conformance Suite (`npx tsx`)**:
   - Command: `npx tsx src/a11y/__tests__/a11y-i18n.test.ts` (in `d:\llmos\desktop`)
   - Result: **ALL 7 SUITES PASSED (148 deterministic assertions verified)**.
3. **Rust Formatting, Clippy & Integration Conformance Suite (`cargo fmt / clippy / test`)**:
   - Commands:
     - `cargo fmt --manifest-path d:\llmos\desktop\src-tauri\Cargo.toml -- --check`
     - `cargo clippy --manifest-path d:\llmos\desktop\src-tauri\Cargo.toml --tests -- -D warnings`
     - `cargo test --manifest-path d:\llmos\desktop\src-tauri\Cargo.toml --test a11y_i18n_conformance_side`
   - Result: **6 passed, 0 failed**, `0` clippy warnings, `0` formatting diffs.

---

## 3. Honest Boundaries & Limitations (`PARTIAL_PASS`, `H4`)

- Shared shell entrypoints (`desktop/src-tauri/src/lib.rs`, `desktop/src/main.ts`, and `desktop/src/style.css`) are intentionally untouched in this parallel lane to preserve pairwise write-set disjointness (`∩ = ∅`) and will be wired by the wave barrier integrator `W73-INT`.
- Real-OS cross-platform computer-use GUI interaction (`#3 C-GUI-CAMPAIGN` / `U-3`) and the full non-developer multi-application exit journey (`[ROAD-D-001]`) are verified in lanes `W73-D-GUI-CAMPAIGN` and `W74-D-ROADD001`.
