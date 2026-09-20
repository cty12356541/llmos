// Task Space(W33-F)的数据模型:把 Task Manager 读侧的三域恢复巡检
// 回执(InspectHealth/InspectSemanticHealth/InspectResourceHealth——与
// CLI 同字节)投影成「任务行」列表,并维护会话内手动关注(follow)的
// plan_id 集合。零 mutation:一切事实来自既有只读回执,不做第二套推导。

import type { RecoveryAlertDto, ReceiptDto } from "../../types";

export type RecoveryDomain = "artifact" | "semantic" | "resource";

/** 一次三域扫描中单个域的聚合快照(来自一张真实巡检回执)。 */
export interface DomainScan {
  domain: RecoveryDomain;
  /** 该域巡检回执的 to_bytes hex(与 CLI `RECEIPT` 行同一等价契约)。 */
  receiptHex: string;
  /** 回执为类型化失败时的诚实降级:null 表示该域本次无巡检事实。 */
  counts: DomainCounts | null;
  alerts: RecoveryAlertDto[];
}

export interface DomainCounts {
  durableRetrying: number;
  durableEscalated: number;
  durableUnacknowledgedEscalated: number;
  durableResolved: number;
}

/** 任务空间中的一行:一个 plan_id + 它在三域恢复台账上的告警关联。 */
export interface TaskRow {
  planIdHex: string;
  /** scan=三域巡检发现;follow=操作者手动关注(InspectTask 验证过存在)。 */
  source: "scan" | "follow";
  /** 每域最新告警事实(域 → 告警行);缺席即该域台账无此计划。 */
  domains: Map<RecoveryDomain, RecoveryAlertDto>;
}

function domainCounts(receipt: ReceiptDto): DomainCounts | null {
  switch (receipt.outcome.kind) {
    case "inspected":
    case "semantic_inspected":
    case "resource_recovery_inspected":
      return {
        durableRetrying: receipt.outcome.durableRetrying,
        durableEscalated: receipt.outcome.durableEscalated,
        durableUnacknowledgedEscalated: receipt.outcome.durableUnacknowledgedEscalated,
        durableResolved: receipt.outcome.durableResolved,
      };
    default:
      return null;
  }
}

function domainAlerts(receipt: ReceiptDto): readonly RecoveryAlertDto[] {
  switch (receipt.outcome.kind) {
    case "inspected":
    case "semantic_inspected":
    case "resource_recovery_inspected":
      return receipt.outcome.alerts;
    default:
      return [];
  }
}

/** 一张域巡检回执 → DomainScan(失败回执诚实保留 hex,counts/alerts 清空)。 */
export function scanFromReceipt(domain: RecoveryDomain, receipt: ReceiptDto): DomainScan {
  return {
    domain,
    receiptHex: receipt.receiptHex,
    counts: domainCounts(receipt),
    alerts: [...domainAlerts(receipt)],
  };
}

/** 由 follow 集合 + 最新三域扫描重建任务行表(无跨刷新陈旧关联:
 * follow 行恒在(域关联按最新扫描吸收),scan 行只活在最新 escalated 集内)。 */
export function rebuildRows(
  followedPlanIds: Iterable<string>,
  scans: readonly DomainScan[],
): Map<string, TaskRow> {
  const rows = new Map<string, TaskRow>();
  for (const planIdHex of followedPlanIds) {
    rows.set(planIdHex, {
      planIdHex,
      source: "follow",
      domains: new Map<RecoveryDomain, RecoveryAlertDto>(),
    });
  }
  for (const scan of scans) {
    for (const alert of scan.alerts) {
      const key = alert.planIdHex.toLowerCase();
      const row = rows.get(key) ?? {
        planIdHex: key,
        source: "scan" as const,
        domains: new Map<RecoveryDomain, RecoveryAlertDto>(),
      };
      row.domains.set(scan.domain, alert);
      rows.set(key, row);
    }
  }
  return rows;
}
