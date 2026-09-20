import { invoke } from "@tauri-apps/api/core";

import type {
  ConfigDto,
  ControlActionInput,
  ControlPlaneFactsDto,
  FactCheckDto,
  ParityDto,
  ReceiptDto,
  SurfacesPresentationDto,
} from "./types";

export async function getConfig(): Promise<ConfigDto> {
  return invoke("get_config");
}

export interface SetConfigInput {
  socketPath?: string | null;
  principalHex?: string | null;
  keyFile?: string | null;
  cliSocket?: string | null;
  cliPath?: string | null;
  resourceRoot?: string | null;
  applicationRoot?: string | null;
}

export async function setConfig(input: SetConfigInput): Promise<ConfigDto> {
  return invoke("set_config", { input });
}

export async function inspectHealth(): Promise<ReceiptDto> {
  return invoke("inspect_health");
}

export async function inspectSemanticHealth(): Promise<ReceiptDto> {
  return invoke("inspect_semantic_health");
}

export async function inspectResourceHealth(): Promise<ReceiptDto> {
  return invoke("inspect_resource_health");
}

export async function exportMetrics(): Promise<ReceiptDto> {
  return invoke("export_metrics");
}

export async function exportSemanticMetrics(): Promise<ReceiptDto> {
  return invoke("export_semantic_metrics");
}

/** W32-E 资源监控:resource 域(G8)OpenMetrics 导出(既有只读命令的 GUI 接线)。 */
export async function exportResourceMetrics(): Promise<ReceiptDto> {
  return invoke("export_resource_metrics");
}

export async function inspectTask(planIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_task", { planIdHex });
}

export async function inspectProcess(processIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_process", { processIdHex });
}

export async function inspectResource(reservationIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_resource", { reservationIdHex });
}

/** W32-D:预算/成本查询(会话 resource_root 接线时组装真实有界成本事实)。 */
export async function inspectResourceCost(reservationIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_resource_cost", { reservationIdHex });
}

/** W32-D 一致性自检:同一 reservation 两次独立认证派发,渲染事实 vs 直接复检。 */
export async function costFactCheck(reservationIdHex: string): Promise<FactCheckDto> {
  return invoke("cost_fact_check", { reservationIdHex });
}

/** W32-D 控制面授权事实(客户端路径常量)。 */
export async function controlPlaneFacts(): Promise<ControlPlaneFactsDto> {
  return invoke("control_plane_facts");
}

/** W32-F:按包身份呈现应用声明的可呈现表面(本地应用权威直读)。 */
export async function presentSurfaces(packageIdHex: string): Promise<SurfacesPresentationDto> {
  return invoke("present_surfaces", { packageIdHex });
}

export async function parityCheck(
  operation: string,
  targetHex?: string | null,
): Promise<ParityDto> {
  return invoke("parity_check", { operation, targetHex: targetHex ?? null });
}

/** W32-B 写路径唯一入口:授权动作 → 真实 ControlCommand → 认证 IPC。 */
export async function submitControl(action: ControlActionInput): Promise<ReceiptDto> {
  return invoke("submit_control", { action });
}

/** 写路径一致性探针(pause-operation):同命令 GUI 认证入口 vs CLI plain 入口。 */
export async function parityCheckWrite(
  commandIdHex: string,
  targetHex: string,
  expectedRevision: number,
  reason: string,
): Promise<ParityDto> {
  return invoke("parity_check_write", {
    commandIdHex,
    targetHex,
    expectedRevision,
    reason,
  });
}
