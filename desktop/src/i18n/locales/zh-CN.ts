// Versioned Simplified Chinese (zh-CN) Workstation Resource Bundle ([TUI-EXT-001]).
// Authority: Stage D Desktop Visual & UX Spec §0.1 (ANTI-AI-05, ANTI-AI-07), §3.7, §4.1.
// Hard Guarantees:
//   - bundleVersion: "1.0.0-stage-d", schemaRevision: 1
//   - Zero pictographic emoji characters across all keys
//   - Zero internal kernel jargon (Z1, TypedIntent, 0x1F, Quota, TaskAttempt, Fiber, Cell, receipt_hex)
//     in all L0/L1/L2 user-facing sections (l3ForensicDag is strictly gated to L3 diagnostic mode)

export const LOCALE_BUNDLE_VERSION = "1.0.0-stage-d" as const;
export const LOCALE_SCHEMA_REVISION = 1 as const;

export type SupportedLocale = "zh-CN" | "en-US";

export type CanonicalTaskStateKey =
  | "PROPOSED"
  | "WAITING_CONSENT"
  | "AUTHORIZED"
  | "RUNNING"
  | "WAITING_INPUT"
  | "WAITING_RESOURCE"
  | "WAITING_NETWORK"
  | "SUSPENDED"
  | "RECOVERING"
  | "CANCELLING"
  | "COMPLETING"
  | "COMPLETED"
  | "PARTIAL"
  | "DISPUTED"
  | "FAILED"
  | "CANCELLED";

export type ExtendedWorkstationStateKey =
  | CanonicalTaskStateKey
  | "DRAFT"
  | "QUARANTINED"
  | "DECLARED"
  | "BLOCKED_DEPENDENCY"
  | "ELIGIBLE"
  | "ACTIVE"
  | "SKIPPED"
  | "SUPERSEDED";

export type ControlCenterDomainKey =
  | "system-device"
  | "workspace"
  | "application"
  | "task"
  | "task-group"
  | "task-node"
  | "task-attempt"
  | "execution-unit"
  | "resource-group"
  | "topic-channel"
  | "operation-device"
  | "cell-distributed-lease";

export interface LocaleResourceBundle {
  readonly locale: SupportedLocale;
  readonly bundleVersion: typeof LOCALE_BUNDLE_VERSION;
  readonly schemaRevision: typeof LOCALE_SCHEMA_REVISION;
  readonly planes: {
    readonly z0Backdrop: string;
    readonly z1TaskSpace: string;
    readonly z2Compositor: string;
    readonly z3Taskbar: string;
    readonly z3CommandPalette: string;
    readonly z3SearchPalette: string;
    readonly z3Notifications: string;
    readonly z4SecurityScreen: string;
  };
  readonly taskStates: Readonly<Record<ExtendedWorkstationStateKey, string>>;
  readonly l2AttributionDrawer: {
    readonly title: string;
    readonly scopeBreadcrumbLabel: string;
    readonly stepSummaryTitle: string;
    readonly resourceUsageBarTitle: string;
    readonly actualUsageLabel: string;
    readonly effectiveAllowanceLabel: string;
    readonly requestedCeilingLabel: string;
    readonly partialCompletionTitle: string;
    readonly closeDrawerAction: string;
    readonly statutoryLockSectionTitle: string;
    readonly statutoryLocks: {
      readonly searchReadOnly: string;
      readonly irreversibleNoUndo: string;
      readonly nodeCompletedOrActiveLocked: string;
      readonly attemptNoManualCommit: string;
      readonly operationEffectUnknownNoManualSuccess: string;
      readonly isolatedLeaseFence: string;
    };
  };
  readonly controlCenterDomains: Readonly<Record<ControlCenterDomainKey, string>>;
  readonly controlCenterActions: {
    readonly flyoutTitle: string;
    readonly taskManagerTitle: string;
    readonly resourceMonitorTitle: string;
    readonly performanceBalanced: string;
    readonly performanceQuietEco: string;
    readonly performanceHighThroughput: string;
    readonly hardwareMicrophone: string;
    readonly hardwareCamera: string;
    readonly hardwareScreenCapture: string;
    readonly hardwareLocation: string;
    readonly hardwareDisconnectAction: string;
    readonly sharedProcessFaultDomainBadge: string;
    readonly blastRadiusPreviewTitle: string;
    readonly twoStepArmingPromptTemplate: string;
    readonly dataLossAcknowledgeLabel: string;
    readonly commandReconcilingNotice: string;
    readonly settingsFourLayerTitle: string;
    readonly associationsTitle: string;
    readonly keychainVaultTitle: string;
    readonly lockWorkstationAction: string;
    readonly themeGraphite: string;
    readonly themePaper: string;
    readonly themeSystem: string;
  };
  readonly z4SecurityConsent: {
    readonly dialogTitle: string;
    readonly verificationSealLabel: string;
    readonly subjectRowLabel: string;
    readonly actionRowLabel: string;
    readonly scopeRowLabel: string;
    readonly impactRowLabel: string;
    readonly reversibleBadge: string;
    readonly irreversibleWarning: string;
    readonly armingCountdownPrompt: string;
    readonly approveAction: string;
    readonly denyAction: string;
    readonly chooserTitle: string;
    readonly taintTransferTitle: string;
  };
  readonly notifications: {
    readonly flyoutTitle: string;
    readonly emptyFeedText: string;
    readonly oneTimeActionUsed: string;
    readonly oneTimeActionExpired: string;
    readonly dismissAction: string;
    readonly clearAllAction: string;
    readonly unreadCountTemplate: string;
  };
  readonly spotlight: {
    readonly inputPlaceholder: string;
    readonly readOnlyPreviewBanner: string;
    readonly partialRemoteTimeoutBanner: string;
    readonly jumpToOriginalAction: string;
    readonly mutationBlockedNotice: string;
    readonly resultCountTemplate: string;
  };
  readonly offlineAutonomy: {
    readonly localReadyBanner: string;
    readonly offlineAutonomyActiveBanner: string;
    readonly degradedHeartbeatBanner: string;
    readonly partitionedBanner: string;
    readonly fencedReadOnlyBanner: string;
    readonly isolatedHoldBanner: string;
    readonly pendingSyncCountTemplate: string;
  };
  readonly a11yAnnouncements: {
    readonly landmarks: {
      readonly workspaceMain: string;
      readonly taskSpaceRegion: string;
      readonly applicationWindowRegionTemplate: string;
      readonly inspectorComplementary: string;
      readonly taskbarContentinfo: string;
      readonly commandPaletteDialog: string;
      readonly searchPaletteDialog: string;
      readonly notificationRegion: string;
      readonly securityAlertDialog: string;
    };
    readonly focusTrappedInSecurityDialog: string;
    readonly focusRestoredToTrigger: string;
    readonly localeSwitchedAnnouncement: string;
    readonly reducedMotionEnabledAnnouncement: string;
    readonly reducedMotionDisabledAnnouncement: string;
    readonly taskStateTransitionPoliteTemplate: string;
    readonly securityPromptAssertiveTemplate: string;
    readonly isolatedAlertAssertiveTemplate: string;
    readonly rovingGridPositionTemplate: string;
  };
  readonly l3ForensicDag: {
    readonly sectionTitle: string;
    readonly taskPlanDagLabel: string;
    readonly lazySupernodeSummaryTemplate: string;
    readonly attemptRaceLaneLabel: string;
    readonly fiberWaterfallLabel: string;
    readonly cellLeaseInspectorLabel: string;
    readonly typedIntentPreviewLabel: string;
    readonly searchNonAuthoritativeMaskTag: string;
    readonly receiptHexAuditLabel: string;
    readonly cliParityVerifiedBadge: string;
  };
}

export const ZH_CN_BUNDLE: LocaleResourceBundle = {
  locale: "zh-CN",
  bundleVersion: LOCALE_BUNDLE_VERSION,
  schemaRevision: LOCALE_SCHEMA_REVISION,
  planes: {
    z0Backdrop: "工作站桌面底板",
    z1TaskSpace: "任务与承诺工作区",
    z2Compositor: "应用窗口工作区",
    z3Taskbar: "工作站底部任务栏",
    z3CommandPalette: "命令与自然语言意图面板",
    z3SearchPalette: "全局候选搜索面板",
    z3Notifications: "系统通知与快捷确认面板",
    z4SecurityScreen: "系统安全确认屏",
  },
  taskStates: {
    DRAFT: "草稿待提交",
    PROPOSED: "草稿待提交",
    WAITING_CONSENT: "待您确认权限或费用上限",
    AUTHORIZED: "正在运行",
    RUNNING: "正在运行",
    WAITING_INPUT: "待补充说明",
    WAITING_RESOURCE: "排队等待计算资源",
    WAITING_NETWORK: "等待网络连接（本地正常可用）",
    SUSPENDED: "已暂停",
    RECOVERING: "正在对账恢复",
    CANCELLING: "正在取消",
    COMPLETING: "正在完成收尾",
    COMPLETED: "已完成",
    PARTIAL: "部分完成（含未完成项）",
    DISPUTED: "结果不一致，待您核对",
    FAILED: "未能完成",
    CANCELLED: "已取消",
    QUARANTINED: "已受控隔离",
    DECLARED: "待执行步骤",
    BLOCKED_DEPENDENCY: "等待前置步骤完成",
    ELIGIBLE: "已就绪待启动",
    ACTIVE: "正在执行",
    SKIPPED: "已跳过",
    SUPERSEDED: "已由新尝试替代",
  },
  l2AttributionDrawer: {
    title: "任务执行详情与资源归因抽屉",
    scopeBreadcrumbLabel: "当前归属作用域路径",
    stepSummaryTitle: "执行步骤与进展明细",
    resourceUsageBarTitle: "三水位资源使用情况",
    actualUsageLabel: "当前实际使用",
    effectiveAllowanceLabel: "当前生效授予额度",
    requestedCeilingLabel: "任务申请上限",
    partialCompletionTitle: "部分完成目标与未执行项明细",
    closeDrawerAction: "关闭详情抽屉",
    statutoryLockSectionTitle: "系统安全只读保护规则",
    statutoryLocks: {
      searchReadOnly: "候选搜索预览只读 · 打开原记录核实后可操作",
      irreversibleNoUndo: "不可逆外部操作 · 无撤销入口，可查看操作凭证",
      nodeCompletedOrActiveLocked: "已执行或正在执行的步骤禁止修改结构",
      attemptNoManualCommit: "系统自动仲裁最终提交 · 不可手动标记完成",
      operationEffectUnknownNoManualSuccess: "外部执行结果待对账核实 · 不可手动改判成功",
      isolatedLeaseFence: "安全围栏隔离中 · 待收齐关闭确认后解锁",
    },
  },
  controlCenterDomains: {
    "system-device": "系统与主机设备",
    "workspace": "工作区与租户环境",
    "application": "已安装应用程序",
    "task": "用户任务与承诺",
    "task-group": "任务编组集合",
    "task-node": "计划执行步骤节点",
    "task-attempt": "单次执行运行记录",
    "execution-unit": "宿主进程与执行单元",
    "resource-group": "计算与存储资源池",
    "topic-channel": "消息主题与数据通道",
    "operation-device": "外部设备与副作用操作",
    "cell-distributed-lease": "跨节点租约与连接态势",
  },
  controlCenterActions: {
    flyoutTitle: "系统控制中心",
    taskManagerTitle: "十二域统一任务与归属管理器",
    resourceMonitorTitle: "三水位资源与节点健康监控台",
    performanceBalanced: "均衡模式",
    performanceQuietEco: "节能静音",
    performanceHighThroughput: "高性能吞吐",
    hardwareMicrophone: "麦克风输入权限",
    hardwareCamera: "摄像头采集权限",
    hardwareScreenCapture: "屏幕画面录制权限",
    hardwareLocation: "地理位置读取权限",
    hardwareDisconnectAction: "立即切断硬件占用",
    sharedProcessFaultDomainBadge: "共享宿主进程故障域",
    blastRadiusPreviewTitle: "级联操作影响范围预览",
    twoStepArmingPromptTemplate: "二次确认已就绪，请在 {seconds} 秒内再次点击确认执行",
    dataLossAcknowledgeLabel: "我已知晓清空通道将永久丢弃未消费消息并生成数据损失回执",
    commandReconcilingNotice: "正在核对上一次控制命令的执行状态，核实前禁止重复提交",
    settingsFourLayerTitle: "四层作用域设置中心",
    associationsTitle: "内容类型与默认打开方式管理",
    keychainVaultTitle: "密钥引用保险箱（零明文驻留）",
    lockWorkstationAction: "立即锁定工作站会话",
    themeGraphite: "石墨深色工作站",
    themePaper: "纸白浅色工作站",
    themeSystem: "跟随系统外观",
  },
  z4SecurityConsent: {
    dialogTitle: "系统安全授权确认",
    verificationSealLabel: "硬件级系统安全防伪印章",
    subjectRowLabel: "发起主体与应用身份",
    actionRowLabel: "请求执行的敏感操作",
    scopeRowLabel: "受控目标范围与资源上限",
    impactRowLabel: "可逆性分级与后代影响",
    reversibleBadge: "可逆操作 · 支持一键回滚",
    irreversibleWarning: "不可逆高危操作 · 执行后无法自动撤销",
    armingCountdownPrompt: "防误触保护开启中，请核对上方四行信息后确认",
    approveAction: "批准并签发授权",
    denyAction: "拒绝并终止请求",
    chooserTitle: "受限文件与资源选择器",
    taintTransferTitle: "跨应用受控剪贴板与拖拽确认",
  },
  notifications: {
    flyoutTitle: "系统通知中心",
    emptyFeedText: "当前没有待处理的系统通知",
    oneTimeActionUsed: "该快捷操作凭证已核销使用",
    oneTimeActionExpired: "该快捷操作凭证已过期失效",
    dismissAction: "忽略此通知",
    clearAllAction: "清空已读通知",
    unreadCountTemplate: "共 {count} 条未读通知",
  },
  spotlight: {
    inputPlaceholder: "搜索任务、应用、工作区或历史记录（按回车打开原记录）",
    readOnlyPreviewBanner: "候选搜索预览 · 回车打开原记录核实",
    partialRemoteTimeoutBanner: "部分远端节点未响应，当前仅显示已返回结果",
    jumpToOriginalAction: "打开权威原记录核实",
    mutationBlockedNotice: "搜索候选预览仅支持只读浏览，请先打开原记录后再执行修改操作",
    resultCountTemplate: "匹配到 {count} 项候选记录",
  },
  offlineAutonomy: {
    localReadyBanner: "本地就绪 · 单机与本地自治功能完整可用",
    offlineAutonomyActiveBanner: "离线自治模式运行中 · 本地任务与安全策略正常执行",
    degradedHeartbeatBanner: "远端连接心跳延迟升高 · 本地操作不受影响",
    partitionedBanner: "网络分区隔离中 · 已切换为本地自治只读保护",
    fencedReadOnlyBanner: "节点处于安全只读围栏保护态",
    isolatedHoldBanner: "节点已被安全围栏隔离 · 待收齐关闭凭证后恢复",
    pendingSyncCountTemplate: "待网络恢复后同步的本地提交：{count} 项",
  },
  a11yAnnouncements: {
    landmarks: {
      workspaceMain: "工作站主工作区",
      taskSpaceRegion: "任务与承诺工作区表格",
      applicationWindowRegionTemplate: "应用窗口：{title}",
      inspectorComplementary: "任务详情与资源归因抽屉",
      taskbarContentinfo: "工作站底部状态与任务栏",
      commandPaletteDialog: "命令与意图输入对话框",
      searchPaletteDialog: "全局候选搜索对话框",
      notificationRegion: "系统通知横幅与列表区",
      securityAlertDialog: "系统安全确认模态对话框",
    },
    focusTrappedInSecurityDialog: "当前处于系统安全确认框，键盘焦点已锁定在此对话框内",
    focusRestoredToTrigger: "对话框已关闭，键盘焦点已恢复至原触发控件",
    localeSwitchedAnnouncement: "界面语言已切换为简体中文（zh-CN）",
    reducedMotionEnabledAnnouncement: "已启用减弱动态效果模式，所有微动效与示波印章已冻结为静态帧",
    reducedMotionDisabledAnnouncement: "已恢复标准工业工作站动态过渡模式",
    taskStateTransitionPoliteTemplate: "任务「{taskTitle}」状态已更新为：{stateLabel}",
    securityPromptAssertiveTemplate: "系统安全提醒：操作「{actionName}」需要您的明确授权确认",
    isolatedAlertAssertiveTemplate: "安全围栏警报：对象「{targetName}」已进入受控隔离状态",
    rovingGridPositionTemplate: "第 {row} 行，共 {totalRows} 行；第 {col} 列",
  },
  l3ForensicDag: {
    sectionTitle: "L3 深度诊断与取证视图 (Forensic & DAG Inspector)",
    taskPlanDagLabel: "TaskPlan 正交有向无环图与惰性物化超节点",
    lazySupernodeSummaryTemplate: "[+{count} 待物化 TaskNode · {activeCount} 活跃 Process]",
    attemptRaceLaneLabel: "TaskAttempt 竞速泳道 (CAS-Only Commit [TASK-COMMIT-001])",
    fiberWaterfallLabel: "ExecutionFiber 五段相位堆栈瀑布条",
    cellLeaseInspectorLabel: "Stage C Cell 三族租约 (Quota / Capacity / ExclusiveDevice) 与 LeaseEpoch",
    typedIntentPreviewLabel: "TypedIntent 编译载荷与 ActionProposal 结构",
    searchNonAuthoritativeMaskTag: "NON_AUTHORITATIVE · mask=0x1F [CONS-CTRL-001]",
    receiptHexAuditLabel: "SABI ControlReceipt 原始字节 (receipt_hex) 与 CLI 等价核验",
    cliParityVerifiedBadge: "GUI receipt_hex == CLI stdout 字节级等价已验证",
  },
};
