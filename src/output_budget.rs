//! DES-12 整批工具输出预算（batch output budget）。
//!
//! 既有实现的缺口：`BudgetHook::fit` 只按“历史后缀 + 当前 prompt”做摘要/裁剪，
//! 且 `read` 的 `input_limit / 4`、Shell 的固定 32 KiB 都是**单条**上限，不是
//! **整批**上限。当最新一批（并发）工具结果的正文本身就超过剩余输入预算时，
//! 摘要无法削减 prompt，`fit` 只能 `Stop`。
//!
//! 本模块在**真实 provider 请求发出之前**、`on_dispatch` 边界上工作：此时请求
//! 的 `chat_history` 已经包含全部真实工具结果（历史批次 + 当前批次的 prompt），
//! 且这是唯一能在不改动 `mio-agent` 无 I/O 引擎的前提下重写 **prompt** 的挂点
//! （`RequestPatch::history` 只替换历史，prompt 永远由引擎追加在最后）。
//!
//! 规则：
//! - 只外置 **工具结果** 里的 `Text` / `Json` 正文；`toolCall`/`result` 身份、
//!   顺序、`name`、`is_error` 与所有 `Image` 富块原样保留。
//! - 外置走真实 `OutputStore` 捕获事务：内容寻址命名、登记成功落盘后才发布
//!   typed [`OutputReference`]；事务未 commit 前失败会回滚，不留孤儿文件。
//! - 复用索引/缓存条目之前，必须在捕获租约下用**磁盘真实普通（非符号链接）文件**
//!   的长度与 SHA-256 复核；索引或内存缓存里的 hash/bytes 只是声称，不能证明正文。
//!   文件被删除、被同长度改写或被替换成符号链接时一律 fail-closed，不发布陈旧引用，
//!   也不悄悄覆盖未知的既有文件。
//! - 只有本 run 的 canonical [`ToolCallTracker`] 里存在真实 `providerId` 的工具
//!   调用才外置；历史里身份不明的结果不猜、不伪造，宁可保守拒绝。
//! - 同一个 run 内重复 prepare（重试、后续轮次都会基于未改动的持久历史重建请求）
//!   通过有界内容寻址缓存 + 索引真实 hash/字节复核复用同一个文件，不每轮新建。
//! - 整批不是“每条固定 2 KiB 预览”：先按**最小引用元数据**（零内联预览）规划，
//!   只要最小元数据能放进预算，就把整批剩余空间用有界二分分配成**统一的 UTF-8
//!   安全内联预览上限**；只有连最小元数据都放不下时，才在零 Provider / 零 I/O 下
//!   判为不可约简。非收益替换（元数据比正文更大）不选入计划。
//! - 发布按**调用分组**：同一调用发布完成并成功补进 canonical 记录（Host 回调成功）
//!   后才 `keep` 这些文件；更晚的调用失败只回滚尚未被引用的新建文件，已经由真实
//!   canonical 记录引用的文件永不删除（Host 回调失败可能已部分持久，保守保留）。
//! - 发布意图账本：在文件/index 都已登记、真实 Host 回调**之前**写入 durable
//!   `Pending` 意图；Host 成功则注销意图并把引用移交常规 journal/session 扫描，Host
//!   `Err` 则保持 `PendingUncertain`（清理前精确保护其 typed 引用，错误文本携带真实
//!   attempt/snapshot 供显式 Host 对账）。意图写入失败时没有任何 Host 副作用，
//!   `CaptureTransaction` 在 `Drop` 中回滚本事务新建文件，不留未登记孤儿。
//! - 8 MiB 单文件硬上限在**任何 I/O 之前**校验；超限正文不进入候选，更不会写盘。
//! - 小请求零 I/O：请求在预算内时不做任何磁盘访问，也不创建 `.mio`。
//! - 外置后“固定指令 + 最小引用元数据”仍放不下时 fail-closed：回滚本次新文件，
//!   由调用方在零 provider 的前提下 `Stop`。
//! - 媒体（图片等）无法用本策略估算 token，沿用既有行为如实 fail-closed，但
//!   绝不删除媒体块。
//!
//! 有界限制（如实说明，不做数学覆盖声明）：`MAX_BATCH_CANDIDATES` 只是本算法的
//! 有界候选上限（按正文从大到小截断），用于避免病态历史造成重建成本爆炸；它**不
//! 证明**能处理任意调用数量的请求。超过上限时，未进入候选的正文不会被外置，若剩余
//! 预算仍不足则保守 `Stop`，绝不静默丢弃结果。

use crate::shell::output_store::{
    CaptureTransaction, MAX_OUTPUT_BYTES, OutputEntry, OutputReference, OutputStore,
    PublicationResolution,
};
use crate::tool_lifecycle::ToolCallTracker;
use mio_core::completion::{CompletionRequest, Document};
use mio_core::message::{Message, Text, ToolResultContent, UserContent};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

/// 单条正文内联预览的上限（真实预览按整批剩余空间分配，不超过它）。
const PREVIEW_BYTES: usize = 2 * 1024;
/// 结构保形 JSON 收缩的附加框架预算。
const STRUCTURED_JSON_FRAMING: usize = 1024;
/// 统一预览上限的有界二分步数（8 MiB 上限 < 2^23）。
const CAP_SEARCH_STEPS: usize = 24;
/// 内容寻址缓存条目上限（每个 run）。
const MAX_CACHE_ENTRIES: usize = 512;
/// 单次请求参与规划的候选正文上限；按尺寸从大到小取，避免病态历史造成 O(n²)
/// 重建成本。这是有界算法限制，不宣称覆盖任意调用数量的请求。
pub(crate) const MAX_BATCH_CANDIDATES: usize = 256;
/// 批外置的 stream 标签。
const BATCH_STREAM: &str = "tool";

/// 一次请求的整批预算结果。
pub(crate) enum FitOutcome {
    /// 请求已经在预算内，不需要任何改动（零 I/O）。
    Unchanged,
    /// 已用真实外置引用替换工具结果正文后的请求。
    Patched(Box<CompletionRequest>),
}

/// 整批预算失败的原因。
#[derive(Debug)]
pub(crate) enum FitError {
    /// 外置所有可外置正文后仍然超预算（含根本没有可外置正文）。调用方必须在
    /// 零 provider 请求的前提下终止。
    Irreducible { limit: usize },
    /// 真实 I/O / 媒体估算等失败；消息已经是最终诊断文本。
    Message(String),
}

/// 内容寻址缓存里的一条已登记文件。
struct CachedFile {
    path: String,
    total_bytes: u64,
    sha256: String,
}

/// 一个可外置的正文。
struct Candidate {
    message_index: usize,
    content_index: usize,
    item_index: usize,
    call_id: String,
    body: Vec<u8>,
    sha256: String,
    /// 该正文所在的 result（消息下标, 内容下标），用于按 result 合并 Text 候选共享的
    /// 尾随 `output_references` 块成本。
    result_key: (usize, usize),
    /// 只把该正文替换成最小引用元数据（零内联预览）时的序列化字节变化：正数表示
    /// 变小（收益），非正数表示替换并不划算，不应选入计划。
    item_savings: i64,
    /// 该正文是 `Text`（被替换时会追加一个共享的尾随 `output_references` 块）。
    is_text: bool,
}

impl Candidate {
    fn location(&self) -> (usize, usize, usize) {
        (self.message_index, self.content_index, self.item_index)
    }
}

/// 一次请求的整批预算规划结果。
struct Plan {
    /// 参与外置的候选下标（升序）。
    selected: Vec<usize>,
    /// 整批共享的 UTF-8 安全内联预览上限（字节，可为 0）。
    cap: usize,
}

/// 按真实 run 统计整批工具输出预算的写入器。
pub(crate) struct BatchOutputBudget {
    cwd: PathBuf,
    run_id: Uuid,
    tracker: Arc<ToolCallTracker>,
    input_limit: usize,
    /// 与 `context_budget::BudgetHook` 相同的固定成本（固定封装 + preamble +
    /// 工具定义），保证请求级与 dispatch 级使用同一套字节估计。
    fixed_cost: usize,
    cache: Mutex<HashMap<(String, String), CachedFile>>,
}

impl BatchOutputBudget {
    pub(crate) fn new(
        cwd: PathBuf,
        run_id: Uuid,
        tracker: Arc<ToolCallTracker>,
        input_limit: usize,
        fixed_cost: usize,
    ) -> Self {
        Self {
            cwd,
            run_id,
            tracker,
            input_limit,
            fixed_cost,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// 重写 `request` 里超预算的工具结果正文，使其在 `input_limit` 内。
    ///
    /// 见模块文档：只有真实已派发（tracker 可查）的工具调用会被外置；外置在真实
    /// `OutputStore` 事务登记落盘后才发布引用，并**按调用分组**补充到同一
    /// `ToolCallTracker` 的 canonical 记录（只补 refs，不改终态），使 `Host` journal
    /// 能保护清理。
    pub(crate) fn fit_request(&self, request: &CompletionRequest) -> Result<FitOutcome, FitError> {
        let limit = self.input_limit;
        let base_cost = self.request_cost(request).map_err(FitError::Message)?;
        if base_cost <= limit {
            return Ok(FitOutcome::Unchanged);
        }
        let candidates = self.collect_candidates(&request.chat_history);
        // 先在内存里证明“最小引用元数据能放下”；否则在任何 I/O 之前拒绝。
        let Plan { selected, cap } = self.plan(request, &candidates)?;

        // 真实 I/O：只在计划已经能放下时才接触磁盘。
        let store = OutputStore::open(&self.cwd).map_err(|error| {
            FitError::Message(format!(
                "Context budget: cannot open the output store for batch externalization: {error}"
            ))
        })?;
        let mut transaction = CaptureTransaction::begin(&store).map_err(|error| {
            FitError::Message(format!(
                "Context budget: cannot begin the output capture transaction: {error}"
            ))
        })?;
        // Read the index once for the whole plan, and remember files published
        // in this same request, so N bodies never re-read the index N times.
        let index = store.load_index().map_err(|error| {
            FitError::Message(format!(
                "Context budget: cannot read the output index: {error}"
            ))
        })?;
        let existing: HashMap<&str, &OutputEntry> = index
            .entries
            .iter()
            .map(|entry| (entry.path.as_str(), entry))
            .collect();
        let mut just_published: HashMap<String, OutputReference> = HashMap::new();
        let mut published: Vec<(usize, OutputReference)> = Vec::with_capacity(selected.len());
        // 按真实调用分组：一个调用发布 + canonical 补登记（Host 回调）成功后才
        // `keep` 它的文件。更晚的调用失败只回滚尚未被真引用的新建文件。
        for group in group_by_call(&candidates, &selected) {
            let mut references = Vec::with_capacity(group.len());
            for index in &group {
                let reference = self.publish(
                    &store,
                    &mut transaction,
                    &existing,
                    &mut just_published,
                    &candidates[*index],
                )?;
                references.push(reference.clone());
                published.push((*index, reference));
            }
            let call_id = candidates[group[0]].call_id.clone();
            let paths = references
                .iter()
                .map(|reference| reference.path.clone())
                .collect::<Vec<_>>();
            // The files and their index entries exist now, but the Host has not
            // been asked to persist anything yet. Record the durable `Pending`
            // publication intent before the callback so a crash during the
            // callback is recoverable. If this write itself fails there was no
            // Host attempt at all: the transaction `Drop` rolls back this
            // group's fresh files, so no pending evidence needs to survive.
            let intent = match transaction.begin_publication_intent(
                &self.run_id.to_string(),
                Some(&call_id),
                &references,
            ) {
                Ok(intent) => intent,
                Err(error) => {
                    return Err(FitError::Message(format!(
                        "Context budget: cannot record the output publication intent for tool call '{call_id}': {error}"
                    )));
                }
            };
            if let Err(error) = self.supplement_call(&call_id, &references) {
                // Host 回调可能已经部分持久化：保持 PendingUncertain，保留本组已
                // 登记文件（内容寻址、索引可查、可由清理回收），并在错误里携带真实
                // attempt/snapshot，绝不删除可能有真实证据的文件，也绝不谎称 Committed。
                let recorded_state = match transaction.mark_publication_uncertain(&intent) {
                    Ok(Some(updated)) => format!("{:?}", updated.state),
                    // Missing identity or a failed state write both leave the
                    // durable `Pending` entry, which is still conservative and
                    // still only resolvable by an explicit reconciliation.
                    Ok(None) | Err(_) => "Pending".to_string(),
                };
                transaction.keep(&paths);
                return Err(FitError::Message(format!(
                    "{error}; the {} registered output file(s) for tool call '{call_id}' are kept because a Host callback may have partially persisted; publication attempt {} (snapshot {}) is recorded as {recorded_state} and can only be resolved by an explicit host reconciliation",
                    references.len(),
                    intent.attempt_id,
                    intent.snapshot,
                )));
            }
            // Host 明确成功：注销意图，把引用移交给常规 journal/session 扫描。
            let handoff =
                transaction.resolve_publication_intent(&intent, PublicationResolution::Published);
            // The successful Host may already hold these references. A failed
            // handoff must never roll them back or silently continue as though
            // the durable publication intent had been finalized.
            transaction.keep(&paths);
            match handoff {
                Ok(Some(_)) => {}
                Ok(None) => {
                    return Err(FitError::Message(format!(
                        "Context budget: output publication attempt {} (snapshot {}) disappeared before handoff; references were accepted by the Host and files are kept, but publication reconciliation is required",
                        intent.attempt_id, intent.snapshot,
                    )));
                }
                Err(error) => {
                    return Err(FitError::Message(format!(
                        "Context budget: could not finalize output publication attempt {} (snapshot {}): {error}; references were accepted by the Host and files are kept; inspect the durable publication ledger before retrying",
                        intent.attempt_id, intent.snapshot,
                    )));
                }
            }
        }
        let patched = patched_request_with_cap(request, &candidates, &published, cap)
            .map_err(FitError::Message)?;
        if self.request_cost(&patched).map_err(FitError::Message)? > limit {
            // 精确规划已经保证能放下；万一发生（例如并发外部改写），尚未 keep 的
            // 文件随事务 `Drop` 回滚，已经被真引用的文件保留。
            drop(transaction);
            return Err(FitError::Irreducible { limit });
        }
        transaction.commit();
        self.remember(&candidates, &published);
        Ok(FitOutcome::Patched(Box::new(patched)))
    }

    /// 在内存里规划：先只按最小引用元数据（cap = 0）筛出真正有收益的候选并证明
    /// 能放进预算，再把整批剩余空间分配成统一的 UTF-8 安全内联预览上限。
    fn plan(
        &self,
        request: &CompletionRequest,
        candidates: &[Candidate],
    ) -> Result<Plan, FitError> {
        let limit = self.input_limit;
        if candidates.is_empty() {
            return Err(FitError::Irreducible { limit });
        }
        let selected = beneficial_candidates(candidates);
        if selected.is_empty() {
            return Err(FitError::Irreducible { limit });
        }
        // 收益候选全选是成本最低的集合；若它在 cap = 0 都放不下，任何子集也放不下。
        let references = provisional_plan(candidates, &selected);
        let metadata_only = patched_request_with_cap(request, candidates, &references, 0)
            .map_err(FitError::Message)?;
        if self
            .request_cost(&metadata_only)
            .map_err(FitError::Message)?
            > limit
        {
            return Err(FitError::Irreducible { limit });
        }
        let cap = self.distribute_preview(request, candidates, &selected, limit)?;
        Ok(Plan { selected, cap })
    }

    /// 在最小元数据已经放下的集合上，把整批剩余空间分配成一个统一的 UTF-8 安全
    /// 内联预览上限：有界二分求最大可放下的 cap，再做精确复核；JSON 结构回退
    /// 可能让成本非单调，复核失败就折半退到已经证明可放下的 cap = 0。
    fn distribute_preview(
        &self,
        request: &CompletionRequest,
        candidates: &[Candidate],
        selected: &[usize],
        limit: usize,
    ) -> Result<usize, FitError> {
        let references = provisional_plan(candidates, selected);
        let max = selected
            .iter()
            .map(|&index| candidates[index].body.len().min(PREVIEW_BYTES))
            .max()
            .unwrap_or(0);
        let mut low = 0usize;
        let mut high = max;
        for _ in 0..CAP_SEARCH_STEPS {
            if low >= high {
                break;
            }
            let mid = low + (high - low).div_ceil(2);
            let patched = patched_request_with_cap(request, candidates, &references, mid)
                .map_err(FitError::Message)?;
            if self.request_cost(&patched).map_err(FitError::Message)? <= limit {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        let mut cap = low;
        loop {
            let patched = patched_request_with_cap(request, candidates, &references, cap)
                .map_err(FitError::Message)?;
            if self.request_cost(&patched).map_err(FitError::Message)? <= limit {
                return Ok(cap);
            }
            if cap == 0 {
                return Err(FitError::Irreducible { limit });
            }
            cap /= 2;
        }
    }

    /// 请求的保守字节成本，与 `context_budget::BudgetHook` 使用**同一套**估计：
    /// `fixed_cost`（固定封装 + preamble + 工具定义）+ 每条历史消息 + documents。
    /// 媒体无法用该策略估算，如实报错，绝不删除媒体。
    ///
    /// 引擎把 run preamble 作为**首条 System 消息**放进请求，而 `fixed_cost` 已经
    /// 为它（含固定封装与工具定义）预留；这里跳过它，否则同内容的 dispatch 估计会
    /// 比请求级估计更严，旧边界会被无谓地判为超预算或触发外置。
    fn request_cost(&self, request: &CompletionRequest) -> Result<usize, String> {
        let mut messages = request.chat_history.iter();
        if matches!(request.chat_history.first(), Some(Message::System { .. })) {
            messages.next();
        }
        let total = self
            .fixed_cost
            .saturating_add(self.messages_cost(messages)?);
        Ok(total.saturating_add(document_cost(&request.documents)?))
    }

    /// 一批消息的保守字节成本，不含 `fixed_cost` 与 documents。
    fn messages_cost<'a>(
        &self,
        messages: impl IntoIterator<Item = &'a Message>,
    ) -> Result<usize, String> {
        let mut total = 0usize;
        for message in messages {
            total = total.saturating_add(crate::context_budget::message_cost(message)?);
        }
        Ok(total)
    }

    /// 纯内存：一条消息按 dispatch 事务会采用的最小引用元数据（cap = 0）外置后的
    /// 序列化成本。没有可外置收益时返回 `None`。不落盘、不改 tracker、不发布任何
    /// 引用；临时引用只存在于本次成本计算里。
    pub(crate) fn planned_message_cost(&self, message: &Message) -> Result<Option<usize>, String> {
        let messages = std::slice::from_ref(message);
        let candidates = self.collect_candidates(messages);
        let selected = beneficial_candidates(&candidates);
        if selected.is_empty() {
            return Ok(None);
        }
        let references = provisional_plan(&candidates, &selected);
        let patched = patched_messages_with_cap(messages, &candidates, &references, 0);
        self.messages_cost(patched.iter()).map(Some)
    }

    /// 纯内存：整组消息在 dispatch 事务真正会做的外置之后的最小成本，含本预算的
    /// `fixed_cost` 与调用方已预留的 `reserved`。没有可外置收益时返回 `None`，调用方
    /// 不得把“无可外置”当成“已满足”。全程零磁盘、零 tracker 写入、零引用发布。
    pub(crate) fn externalized_cost(
        &self,
        messages: &[Message],
        reserved: usize,
    ) -> Result<Option<usize>, String> {
        let candidates = self.collect_candidates(messages);
        let selected = beneficial_candidates(&candidates);
        if selected.is_empty() {
            return Ok(None);
        }
        let references = provisional_plan(&candidates, &selected);
        let patched = patched_messages_with_cap(messages, &candidates, &references, 0);
        self.messages_cost(patched.iter()).map(|cost| {
            Some(
                self.fixed_cost
                    .saturating_add(reserved)
                    .saturating_add(cost),
            )
        })
    }

    /// 枚举所有可外置的工具结果正文。历史里身份不明（tracker 查不到真实 providerId）
    /// 的结果不猜、不伪造，直接跳过。超过 8 MiB 硬上限的正文在**任何 I/O 之前**就
    /// 不进入候选。
    fn collect_candidates(&self, messages: &[Message]) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        for (message_index, message) in messages.iter().enumerate() {
            let Message::User { content } = message else {
                continue;
            };
            for (content_index, item) in content.iter().enumerate() {
                let UserContent::ToolResult(result) = item else {
                    continue;
                };
                let Some(provider) = result.call.provider() else {
                    continue;
                };
                let call_id = provider.as_str().to_owned();
                if !self.tracker.has_tool_call_id(&call_id) {
                    continue;
                }
                for (item_index, body_item) in result.content.iter().enumerate() {
                    let (body, is_text) = match body_item {
                        ToolResultContent::Text(text) => (text.text.as_bytes().to_vec(), true),
                        ToolResultContent::Json { value } => match serde_json::to_vec(value) {
                            Ok(bytes) => (bytes, false),
                            Err(_) => continue,
                        },
                        ToolResultContent::Image(_) => continue,
                    };
                    // 8 MiB 单文件硬上限：超限正文不可外置，也不允许写盘。
                    if body.len() as u64 > MAX_OUTPUT_BYTES {
                        continue;
                    }
                    let sha256 = sha256_hex(&body);
                    let mut candidate = Candidate {
                        message_index,
                        content_index,
                        item_index,
                        call_id: call_id.clone(),
                        body,
                        sha256,
                        result_key: (message_index, content_index),
                        item_savings: 0,
                        is_text,
                    };
                    candidate.item_savings = candidate_item_savings(body_item, &candidate);
                    candidates.push(candidate);
                }
            }
        }
        // 大正文优先，能用更少的外置收敛预算；上限只是有界算法限制。
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.body.len()));
        candidates.truncate(MAX_BATCH_CANDIDATES);
        candidates
    }

    /// 把一个正文发布为真实外置文件并返回已验证的 typed 引用。
    ///
    /// 内容寻址命名 + **磁盘真实普通文件长度/SHA-256 复核**：同一内容复用同一个
    /// 文件（缓存或索引），但缓存与索引里的 hash/bytes 只是声称，必须先在捕获租约下
    /// 用磁盘内容证明；文件缺失、同长度改写或符号链接替换一律 fail-closed，不发布
    /// 陈旧引用，也不悄悄覆盖未知的既有文件。
    fn publish(
        &self,
        store: &OutputStore,
        transaction: &mut CaptureTransaction<'_>,
        existing: &HashMap<&str, &OutputEntry>,
        just_published: &mut HashMap<String, OutputReference>,
        candidate: &Candidate,
    ) -> Result<OutputReference, FitError> {
        let name = format!("{}.{}", candidate.sha256, BATCH_STREAM);
        let relative = format!(".mio/outputs/{name}");
        if let Some(reference) = just_published.get(&relative) {
            // The same content appeared earlier in this same plan; reuse the
            // identical file but keep this call's own identity.
            let mut reference = reference.clone();
            reference.call_id = Some(candidate.call_id.clone());
            reference.validate().map_err(|error| {
                FitError::Message(format!(
                    "Context budget: refusing to publish an invalid output reference: {error}"
                ))
            })?;
            return Ok(reference);
        }
        let key = (candidate.sha256.clone(), BATCH_STREAM.to_owned());
        if let Some(cached) = self.cache_get(&key) {
            transaction
                .verify(&cached.path, cached.total_bytes, &cached.sha256)
                .map_err(|error| {
                    FitError::Message(format!(
                        "Context budget: cached externalized output {} failed disk verification: {error}",
                        cached.path
                    ))
                })?;
            return reference_from_cache(&cached, candidate);
        }
        if let Some(entry) = existing.get(relative.as_str()) {
            if entry.hash.as_deref() != Some(candidate.sha256.as_str())
                || entry.bytes != Some(candidate.body.len() as u64)
            {
                return Err(FitError::Message(format!(
                    "Context budget: content-addressed output {relative} is registered with a different hash; refusing to overwrite"
                )));
            }
            transaction
                .verify(
                    &relative,
                    candidate.body.len() as u64,
                    &candidate.sha256,
                )
                .map_err(|error| {
                    FitError::Message(format!(
                        "Context budget: registered externalized output {relative} failed disk verification: {error}"
                    ))
                })?;
            return reference_from_entry(entry, candidate);
        }
        // 新文件：在写盘之前先登记本事务的所有权，登记失败就地回滚，避免留下
        // 未登记孤儿；已经存在的同内容文件不被认领、不被覆盖。
        let path = store.output_dir().join(&name);
        if !real_regular_file(&path) {
            transaction.enroll_fresh(relative.clone(), candidate.sha256.clone());
        }
        let created = write_published(&path, &candidate.body).map_err(FitError::Message)?;
        if !created {
            transaction.forget_fresh(&relative);
        }
        let entry = OutputEntry {
            path: relative.clone(),
            bytes: Some(candidate.body.len() as u64),
            hash: Some(candidate.sha256.clone()),
            stream: BATCH_STREAM.to_owned(),
            created: 0,
            call_id: Some(candidate.call_id.clone()),
        };
        let stored = transaction.register(entry).map_err(|error| {
            FitError::Message(format!(
                "Context budget: cannot register externalized output: {error}"
            ))
        })?;
        let reference = reference_from_entry(&stored, candidate)?;
        just_published.insert(relative, reference.clone());
        Ok(reference)
    }

    /// 把一组刚发布并登记成功的引用补进同一 `ToolCallTracker` 的 canonical 记录
    /// （只补 refs，不改终态），并触发真实 Host 持久化。失败时调用方保守保留文件，
    /// 绝不删除可能已经被 Host 部分持久化的证据。
    fn supplement_call(&self, call_id: &str, references: &[OutputReference]) -> Result<(), String> {
        let run_scope = self.run_id.to_string();
        self.tracker
            .supplement_output_references(&run_scope, call_id, references, false)
            .map_err(|error| {
                format!(
                    "Context budget: cannot register externalized output references for tool call '{call_id}': {error}"
                )
            })
    }

    /// 把已 commit 的文件写进内存缓存，供同一 run 的后续 prepare 复用。
    fn remember(&self, candidates: &[Candidate], published: &[(usize, OutputReference)]) {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.len().saturating_add(published.len()) > MAX_CACHE_ENTRIES {
            cache.clear();
        }
        for (index, reference) in published {
            let candidate = &candidates[*index];
            cache.insert(
                (candidate.sha256.clone(), BATCH_STREAM.to_owned()),
                CachedFile {
                    path: reference.path.clone(),
                    total_bytes: reference.total_bytes,
                    sha256: reference.sha256.clone(),
                },
            );
        }
    }

    fn cache_get(&self, key: &(String, String)) -> Option<CachedFile> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
            .map(|cached| CachedFile {
                path: cached.path.clone(),
                total_bytes: cached.total_bytes,
                sha256: cached.sha256.clone(),
            })
    }
}

/// 用候选的真实尺寸构造一个临时引用，只用于在 I/O 之前精确测量请求大小。
fn provisional_reference(candidate: &Candidate) -> OutputReference {
    OutputReference {
        path: format!(".mio/outputs/{}.{}", candidate.sha256, BATCH_STREAM),
        total_bytes: candidate.body.len() as u64,
        stream: BATCH_STREAM.to_owned(),
        call_id: Some(candidate.call_id.clone()),
        sha256: candidate.sha256.clone(),
    }
}

fn provisional_plan(candidates: &[Candidate], selected: &[usize]) -> Vec<(usize, OutputReference)> {
    selected
        .iter()
        .map(|&index| (index, provisional_reference(&candidates[index])))
        .collect()
}

/// 真正能减小请求的最小元数据（cap = 0）收益候选，保持候选原有顺序。
///
/// `Text` 候选共享同一个结果的尾随 `output_references` 块，所以按 result 汇总
/// 收益再与块成本比较；块成本大于收益的整组不选。`Json` 候选把引用嵌在自己的值里，
/// 没有共享块，逐条判定。
fn beneficial_candidates(candidates: &[Candidate]) -> Vec<usize> {
    let mut selected: Vec<usize> = Vec::new();
    let mut text_groups: HashMap<(usize, usize), Vec<usize>> = HashMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        if candidate.is_text {
            text_groups
                .entry(candidate.result_key)
                .or_default()
                .push(index);
        } else if candidate.item_savings > 0 {
            selected.push(index);
        }
    }
    for group in text_groups.into_values() {
        let references = group
            .iter()
            .map(|&index| provisional_reference(&candidates[index]))
            .collect::<Vec<_>>();
        let block = match trailing_block_bytes(&references) {
            Some(block) => block,
            None => continue,
        };
        let savings: i64 = group
            .iter()
            .map(|&index| candidates[index].item_savings)
            .sum();
        if savings > block as i64 {
            selected.extend(group);
        }
    }
    selected.sort_unstable();
    selected
}

/// 把 `Text` 候选被替换后追加的共享引用块编码长度；编码失败时返回 `None`（跳过）。
fn trailing_block_bytes(references: &[OutputReference]) -> Option<usize> {
    serde_json::to_vec(&ToolResultContent::Json {
        value: json!({ "output_references": references }),
    })
    .map(|bytes| bytes.len())
    .ok()
}

/// 单个正文替换成最小引用元数据（cap = 0）时的序列化字节变化：正数表示收益。
fn candidate_item_savings(original: &ToolResultContent, candidate: &Candidate) -> i64 {
    let reference = provisional_reference(candidate);
    let replacement = match original {
        ToolResultContent::Text(text) => {
            ToolResultContent::Text(Text::new(externalized_text(&text.text, &reference, 0)))
        }
        ToolResultContent::Json { value } => ToolResultContent::Json {
            value: shrink_json(value, &reference, 0),
        },
        ToolResultContent::Image(_) => return 0,
    };
    let original_len = match serde_json::to_vec(original) {
        Ok(bytes) => bytes.len(),
        Err(_) => return 0,
    };
    let replacement_len = match serde_json::to_vec(&replacement) {
        Ok(bytes) => bytes.len(),
        Err(_) => return 0,
    };
    original_len as i64 - replacement_len as i64
}

/// 按真实调用 ID 分组选中候选，保持首次出现的顺序。同一调用的正文可能因为按尺寸
/// 排序而互不相邻，因此不能只合并连续段。
fn group_by_call(candidates: &[Candidate], selected: &[usize]) -> Vec<Vec<usize>> {
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for index in selected {
        let call_id = candidates[*index].call_id.clone();
        if !groups.contains_key(&call_id) {
            order.push(call_id.clone());
        }
        groups.entry(call_id).or_default().push(*index);
    }
    order
        .into_iter()
        .filter_map(|call_id| groups.remove(&call_id))
        .collect()
}

/// 真实存在、非符号链接的普通文件。
fn real_regular_file(path: &PathBuf) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata.is_file() && !metadata.file_type().is_symlink(),
        Err(_) => false,
    }
}

fn reference_from_entry(
    entry: &OutputEntry,
    candidate: &Candidate,
) -> Result<OutputReference, FitError> {
    let mut reference = OutputReference::from_entry(entry).map_err(|error| {
        FitError::Message(format!(
            "Context budget: refusing to publish an incomplete output reference: {error}"
        ))
    })?;
    // 索引条目可能由同一内容的上一个调用登记；引用里的 call_id 必须是**本次**
    // 真实调用，不能借用别人的身份。
    reference.call_id = Some(candidate.call_id.clone());
    reference.validate().map_err(|error| {
        FitError::Message(format!(
            "Context budget: refusing to publish an invalid output reference: {error}"
        ))
    })?;
    Ok(reference)
}

fn reference_from_cache(
    cached: &CachedFile,
    candidate: &Candidate,
) -> Result<OutputReference, FitError> {
    let reference = OutputReference {
        path: cached.path.clone(),
        total_bytes: cached.total_bytes,
        stream: BATCH_STREAM.to_owned(),
        call_id: Some(candidate.call_id.clone()),
        sha256: cached.sha256.clone(),
    };
    reference.validate().map_err(|error| {
        FitError::Message(format!(
            "Context budget: cached output reference is invalid: {error}"
        ))
    })?;
    Ok(reference)
}

/// 把 `body` 写成一个新的内容寻址文件。返回 `Ok(true)` 表示本次确实创建了文件；
/// 已存在（同内容或并发先写）时不覆盖、不认领，返回 `Ok(false)`。
fn write_published(path: &PathBuf, body: &[u8]) -> Result<bool, String> {
    let directory = path
        .parent()
        .ok_or_else(|| "output file has no parent directory".to_owned())?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(directory).map_err(|error| error.to_string())?;
    temporary
        .write_all(body)
        .map_err(|error| error.to_string())?;
    temporary.flush().map_err(|error| error.to_string())?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| error.to_string())?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(true),
        // A concurrent writer may have published the identical content-addressed
        // file first; the register step validates its real hash before use.
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

/// 内联预览：UTF-8 安全截断到 `cap` 字节。
fn utf8_preview(text: &str, cap: usize) -> &str {
    if text.len() <= cap {
        return text;
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 正文被外置后的内联文本：`cap` 字节的 UTF-8 安全预览 + 明确截断标记（含真实引用
/// 与总量）。`cap` 由整批剩余空间分配，可为 0。
fn externalized_text(body: &str, reference: &OutputReference, cap: usize) -> String {
    format!(
        "{}\n[TRUNCATED: inline batch output budget; complete output: {}; {} bytes; read it with byte_offset/byte_limit]",
        utf8_preview(body, cap),
        reference.path,
        reference.total_bytes,
    )
}

/// 结构保形的 JSON 收缩：按共享的 `cap` 截断超长字符串并附加引用；结构无法有效
/// 收缩时退化为最小包装（仍然是合法 JSON）。绝不产生非法 JSON，且收缩后的大小
/// 随 `cap` 单调不减（除结构回退点外，调用方会做精确复核）。
fn shrink_json(value: &Value, reference: &OutputReference, cap: usize) -> Value {
    let mut candidate = value.clone();
    truncate_long_strings(&mut candidate, cap);
    if let Value::Object(object) = &mut candidate {
        object.insert("output_references".to_owned(), json!([reference]));
        let size = serde_json::to_vec(&candidate)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if size
            <= cap
                .saturating_mul(2)
                .saturating_add(STRUCTURED_JSON_FRAMING)
        {
            return candidate;
        }
    }
    minimal_json_wrapper(value, reference, cap)
}

fn truncate_long_strings(value: &mut Value, cap: usize) {
    match value {
        Value::String(text) if text.len() > cap => {
            *text = utf8_preview(text, cap).to_owned();
        }
        Value::Array(items) => {
            for item in items {
                truncate_long_strings(item, cap);
            }
        }
        Value::Object(object) => {
            for item in object.values_mut() {
                truncate_long_strings(item, cap);
            }
        }
        _ => {}
    }
}

fn minimal_json_wrapper(value: &Value, reference: &OutputReference, cap: usize) -> Value {
    let text = serde_json::to_string(value).unwrap_or_default();
    json!({
        "output_references": [reference],
        "preview": utf8_preview(&text, cap),
        "truncated": true,
    })
}

/// 用替换表重建消息列表，保持所有身份、顺序与未被替换的块原样；`cap` 是整批共享的
/// UTF-8 安全内联预览上限。
fn patched_messages_with_cap(
    messages: &[Message],
    candidates: &[Candidate],
    references: &[(usize, OutputReference)],
    cap: usize,
) -> Vec<Message> {
    let mut patched = messages.to_vec();
    patch_messages_in_place(&mut patched, candidates, references, cap);
    patched
}

/// 就地应用替换表；调用方已拥有 `messages` 的可变副本。
fn patch_messages_in_place(
    patched: &mut [Message],
    candidates: &[Candidate],
    references: &[(usize, OutputReference)],
    cap: usize,
) {
    let mut replacements: HashMap<(usize, usize, usize), &OutputReference> = HashMap::new();
    for (index, reference) in references {
        replacements.insert(candidates[*index].location(), reference);
    }
    for (message_index, message) in patched.iter_mut().enumerate() {
        let Message::User { content } = message else {
            continue;
        };
        for (content_index, item) in content.iter_mut().enumerate() {
            let UserContent::ToolResult(result) = item else {
                continue;
            };
            // Text bodies carry only the preview + marker inline, so their full
            // reference is appended once as a trailing metadata block (the same
            // shape the tool wrapper already publishes). Json bodies carry the
            // reference inside their own bounded value.
            let mut appended: Vec<OutputReference> = Vec::new();
            for (item_index, body_item) in result.content.iter_mut().enumerate() {
                let Some(reference) = replacements.get(&(message_index, content_index, item_index))
                else {
                    continue;
                };
                match body_item {
                    ToolResultContent::Text(text) => {
                        *text = Text::new(externalized_text(&text.text, reference, cap));
                        appended.push((*reference).clone());
                    }
                    ToolResultContent::Json { value } => {
                        *value = shrink_json(value, reference, cap);
                    }
                    ToolResultContent::Image(_) => {}
                }
            }
            if !appended.is_empty() {
                result.content.push(ToolResultContent::Json {
                    value: json!({ "output_references": appended }),
                });
            }
        }
    }
}

/// 用替换表重建请求，保持所有身份、顺序与未被替换的块原样；`cap` 是整批共享的
/// UTF-8 安全内联预览上限。
fn patched_request_with_cap(
    request: &CompletionRequest,
    candidates: &[Candidate],
    references: &[(usize, OutputReference)],
    cap: usize,
) -> Result<CompletionRequest, String> {
    let mut patched = request.clone();
    patch_messages_in_place(&mut patched.chat_history, candidates, references, cap);
    Ok(patched)
}

fn document_cost(documents: &[Document]) -> Result<usize, String> {
    let mut total = 0usize;
    for document in documents {
        total = total.saturating_add(
            serde_json::to_vec(document)
                .map_err(|_| "Context budget: cannot encode request documents".to_owned())?
                .len(),
        );
    }
    Ok(total)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mio_core::message::{
        AssistantContent, AssistantMessage, DocumentSourceKind, Image, ImageMediaType, ToolCall,
        ToolFunction, ToolName,
    };

    struct TestHost;

    impl crate::Host for TestHost {
        fn shell(
            &self,
            _command: &str,
            _cwd: &std::path::Path,
        ) -> crate::HostFuture<Result<String, String>> {
            Box::pin(async { Ok(String::new()) })
        }
        fn on_event(&self, _event: crate::AgentEvent) {}
    }

    fn run_id() -> Uuid {
        Uuid::new_v4()
    }

    fn tracker_for(run_id: Uuid) -> Arc<ToolCallTracker> {
        ToolCallTracker::new(run_id.to_string(), Arc::new(TestHost))
    }

    fn register_call(tracker: &Arc<ToolCallTracker>, call_id: &str, index: u64) {
        let call_index = tracker
            .begin("read", "read", "file", Some(call_id), Some(1), Some(index))
            .expect("begin records the real provider id");
        tracker
            .admit(call_index, Some(1), Some(index))
            .expect("admit");
        tracker
            .mark_dispatched(index, Some(call_id))
            .expect("mark dispatched");
        tracker
            .observe_outcome(
                index,
                Some(call_id),
                crate::tool_lifecycle::ToolCallPhase::Executed,
                Some("ok"),
            )
            .expect("observe outcome");
    }

    fn call(id: &str) -> ToolCall {
        ToolCall::from_wire(
            id,
            ToolFunction::new(ToolName::new("read").unwrap(), json!({"path": "file"})),
        )
    }

    fn tool_message(id: &str, content: Vec<ToolResultContent>, is_error: bool) -> Vec<Message> {
        let call = call(id);
        let result = if is_error {
            call.error_result(content)
        } else {
            call.result(content)
        };
        vec![
            Message::Assistant(AssistantMessage::new(vec![AssistantContent::ToolCall(
                call,
            )])),
            Message::User {
                content: vec![UserContent::ToolResult(result)],
            },
        ]
    }

    fn request_with(messages: Vec<Message>) -> CompletionRequest {
        let mut request = CompletionRequest::new(Message::user("go"));
        request.chat_history = messages;
        request
    }

    fn budget(
        cwd: &std::path::Path,
        run: Uuid,
        tracker: Arc<ToolCallTracker>,
        limit: usize,
    ) -> BatchOutputBudget {
        // 512 matches `BudgetHook`'s own framing allowance for an empty preamble
        // and empty tool list, so unit limits stay comparable across both.
        BatchOutputBudget::new(cwd.to_path_buf(), run, tracker, limit, 512)
    }

    fn body_bytes(path: &std::path::Path, reference: &OutputReference) -> Vec<u8> {
        let name = reference.path.strip_prefix(".mio/outputs/").unwrap();
        std::fs::read(path.join(".mio/outputs").join(name)).expect("externalized file")
    }

    #[test]
    fn oversized_tool_text_is_externalized_and_reconstructable() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "call-1", 1);
        let text = "行中文".repeat(2_000); // 9000 multibyte bytes
        let request = request_with(tool_message(
            "call-1",
            vec![ToolResultContent::text(text.clone())],
            false,
        ));
        let budget = budget(dir.path(), run, tracker.clone(), 4096);
        let FitOutcome::Patched(patched) = budget.fit_request(&request).unwrap() else {
            panic!("an oversized tool result must be externalized");
        };
        let Message::User { content } = &patched.chat_history[1] else {
            panic!("tool result position");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result");
        };
        assert!(!result.is_error);
        let ToolResultContent::Text(inline) = &result.content[0] else {
            panic!("text");
        };
        assert!(
            !inline.text.contains('\u{FFFD}'),
            "no replacement characters"
        );
        assert!(inline.text.starts_with(&text[..30]));
        assert!(
            inline
                .text
                .contains("[TRUNCATED: inline batch output budget;")
        );
        let reference = tracker.calls()[0]
            .output_references
            .first()
            .cloned()
            .expect("supplemented reference");
        assert_eq!(body_bytes(dir.path(), &reference), text.as_bytes());
        assert_eq!(reference.total_bytes, text.len() as u64);
        assert_eq!(reference.sha256, sha256_hex(text.as_bytes()));
        // The canonical record kept its real phase; only references were added.
        assert_eq!(
            tracker.calls()[0].phase,
            crate::tool_lifecycle::ToolCallPhase::Executed
        );
    }

    #[test]
    fn oversized_tool_json_stays_valid_and_reconstructs_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "call-json", 1);
        let value = json!({"output": "数据".repeat(3_000), "kind": "read"});
        let serialized = serde_json::to_vec(&value).unwrap();
        let request = request_with(tool_message(
            "call-json",
            vec![ToolResultContent::json(value.clone())],
            true,
        ));
        let budget = budget(dir.path(), run, tracker.clone(), 4096);
        let FitOutcome::Patched(patched) = budget.fit_request(&request).unwrap() else {
            panic!("an oversized json result must be externalized");
        };
        let Message::User { content } = &patched.chat_history[1] else {
            panic!("tool result position");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result");
        };
        assert!(result.is_error, "error semantics survive externalization");
        let ToolResultContent::Json { value } = &result.content[0] else {
            panic!("json stays json");
        };
        // Still a parseable JSON object with the reference metadata.
        let object = value.as_object().expect("json object");
        assert!(object.contains_key("output_references"));
        assert!(
            serde_json::to_vec(value).unwrap().len() < serialized.len(),
            "the shrunk JSON must be smaller than the original body"
        );
        let reference = tracker.calls()[0]
            .output_references
            .first()
            .cloned()
            .expect("supplemented reference");
        assert_eq!(body_bytes(dir.path(), &reference), serialized);
        assert_eq!(reference.total_bytes, serialized.len() as u64);
    }

    #[test]
    fn replacement_preserves_image_blocks_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "call-rich", 1);
        let image = Image {
            data: DocumentSourceKind::Base64("Zm9vYmFy".into()),
            media_type: Some(ImageMediaType::PNG),
            detail: None,
            native: None,
        };
        let request = request_with(tool_message(
            "call-rich",
            vec![
                ToolResultContent::text("A".repeat(4_000)),
                ToolResultContent::Image(image.clone()),
                ToolResultContent::json(json!({"output": "B".repeat(4_000)})),
            ],
            false,
        ));
        let candidates =
            budget(dir.path(), run, tracker, 1024).collect_candidates(&request.chat_history);
        assert_eq!(
            candidates.len(),
            2,
            "text and json are externalizable candidates"
        );
        let references: Vec<(usize, OutputReference)> = candidates
            .iter()
            .enumerate()
            .map(|(index, candidate)| (index, provisional_reference(candidate)))
            .collect();
        let patched =
            patched_request_with_cap(&request, &candidates, &references, PREVIEW_BYTES).unwrap();
        let Message::User { content } = &patched.chat_history[1] else {
            panic!("tool result position");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result");
        };
        assert_eq!(
            result.content.len(),
            4,
            "two replaced bodies keep their places and one trailing reference block is appended"
        );
        assert_eq!(
            result.content[1],
            ToolResultContent::Image(image),
            "the image block is byte-for-byte unchanged and in place"
        );
        assert!(matches!(result.content[0], ToolResultContent::Text(_)));
        assert!(matches!(result.content[2], ToolResultContent::Json { .. }));
        assert!(matches!(result.content[3], ToolResultContent::Json { .. }));
    }

    #[test]
    fn repeated_prepare_reuses_the_same_content_addressed_file() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "call-reuse", 1);
        let request = request_with(tool_message(
            "call-reuse",
            vec![ToolResultContent::json(
                json!({"output": "x".repeat(8_000)}),
            )],
            false,
        ));
        let budget = budget(dir.path(), run, tracker.clone(), 4096);
        let FitOutcome::Patched(first) = budget.fit_request(&request).unwrap() else {
            panic!("externalized");
        };
        let second = request_with(tool_message(
            "call-reuse",
            vec![ToolResultContent::json(
                json!({"output": "x".repeat(8_000)}),
            )],
            false,
        ));
        let FitOutcome::Patched(second) = budget.fit_request(&second).unwrap() else {
            panic!("externalized");
        };
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap(),
            "the same history externalizes to the same request"
        );
        let files = std::fs::read_dir(dir.path().join(".mio/outputs"))
            .unwrap()
            .filter_map(Result::ok)
            .count();
        assert_eq!(
            files, 1,
            "content-addressed reuse must not create a second file"
        );
    }

    #[test]
    fn irreducible_without_externalizable_bodies_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        let request = request_with(vec![Message::user("y".repeat(20_000))]);
        let budget = budget(dir.path(), run, tracker, 512);
        assert!(matches!(
            budget.fit_request(&request),
            Err(FitError::Irreducible { .. })
        ));
        assert!(
            !dir.path().join(".mio").exists(),
            "an irreducible request must not create the output store"
        );
    }

    #[test]
    fn unwritable_output_store_fails_closed_instead_of_dropping_output() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "call-fail", 1);
        let request = request_with(tool_message(
            "call-fail",
            vec![ToolResultContent::text("z".repeat(8_000))],
            false,
        ));
        let budget = budget(file.path(), run, tracker, 4096);
        assert!(matches!(
            budget.fit_request(&request),
            Err(FitError::Message(_))
        ));
    }

    #[test]
    fn media_is_fail_closed_without_deleting_it() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "call-media", 1);
        let image = Image {
            data: DocumentSourceKind::base64("Zm9v"),
            media_type: None,
            detail: None,
            native: None,
        };
        let request = request_with(tool_message(
            "call-media",
            vec![
                ToolResultContent::text("q".repeat(4_000)),
                ToolResultContent::Image(image),
            ],
            false,
        ));
        let budget = budget(dir.path(), run, tracker, 512);
        assert!(matches!(
            budget.fit_request(&request),
            Err(FitError::Message(_))
        ));
        assert!(!dir.path().join(".mio").exists(), "media never reaches I/O");
        // The block is still present in the source request; nothing deleted it.
        let Message::User { content } = &request.chat_history[1] else {
            panic!("tool result position");
        };
        let UserContent::ToolResult(result) = &content[0] else {
            panic!("tool result");
        };
        assert_eq!(result.content.len(), 2);
    }

    #[test]
    fn unknown_history_identity_is_never_fabricated() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        // The tracker knows nothing about `call-foreign`.
        let tracker = tracker_for(run);
        let request = request_with(tool_message(
            "call-foreign",
            vec![ToolResultContent::text("w".repeat(8_000))],
            false,
        ));
        let budget = budget(dir.path(), run, tracker, 512);
        assert!(matches!(
            budget.fit_request(&request),
            Err(FitError::Irreducible { .. })
        ));
        assert!(!dir.path().join(".mio").exists());
    }

    /// One assistant turn may hold several tool calls whose results land in a
    /// single user message (the concurrent batch). The whole message is one
    /// budget, not one budget per result.
    #[test]
    fn one_budget_covers_a_multi_result_batch() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        let mut contents = Vec::new();
        for index in 0..3 {
            register_call(&tracker, &format!("batch-{index}"), index as u64 + 1);
            contents.push(mio_core::message::UserContent::ToolResult(
                call(&format!("batch-{index}")).result(vec![ToolResultContent::json(json!({
                    "output": "b".repeat(4_000),
                    "index": index,
                }))]),
            ));
        }
        let request = request_with(vec![
            Message::Assistant(mio_core::message::AssistantMessage::new(Vec::new())),
            Message::User { content: contents },
        ]);
        let budget = budget(dir.path(), run, tracker.clone(), 8_000);
        let FitOutcome::Patched(patched) = budget.fit_request(&request).unwrap() else {
            panic!("the whole batch must be externalized when it cannot fit");
        };
        let Message::User { content } = &patched.chat_history[1] else {
            panic!("batch position");
        };
        for (index, item) in content.iter().enumerate().take(3) {
            let UserContent::ToolResult(result) = item else {
                panic!("tool result {index}");
            };
            let ToolResultContent::Json { value } = &result.content[0] else {
                panic!("json result {index}");
            };
            assert_eq!(value["index"], json!(index), "order is preserved");
            assert!(
                value.get("output_references").is_some(),
                "each over-budget result carries a reference"
            );
        }
        assert_eq!(tracker.calls().len(), 3);
    }

    /// An older turn's tool result (the retained suffix) and the current prompt
    /// are sized under one batch budget.
    #[test]
    fn one_budget_covers_the_cross_turn_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "old-call", 1);
        register_call(&tracker, "new-call", 2);
        let mut messages = tool_message(
            "old-call",
            vec![ToolResultContent::json(
                json!({"output": "o".repeat(4_000)}),
            )],
            false,
        );
        messages.extend(tool_message(
            "new-call",
            vec![ToolResultContent::json(
                json!({"output": "n".repeat(4_000)}),
            )],
            false,
        ));
        let request = request_with(messages);
        let budget = budget(dir.path(), run, tracker.clone(), 6_000);
        let FitOutcome::Patched(patched) = budget.fit_request(&request).unwrap() else {
            panic!("the retained suffix must be externalized");
        };
        let encoded = serde_json::to_vec(&patched.chat_history).unwrap();
        assert!(
            encoded.len() <= 6_000,
            "the whole history fits: {}",
            encoded.len()
        );
        for call_id in ["old-call", "new-call"] {
            let record = tracker
                .calls()
                .into_iter()
                .find(|record| record.tool_call_id.as_deref() == Some(call_id))
                .unwrap();
            assert!(
                !record.output_references.is_empty(),
                "the cross-turn call {call_id} is protected"
            );
        }
    }

    struct Prepared {
        dir: tempfile::TempDir,
        run: Uuid,
        tracker: Arc<ToolCallTracker>,
        request: CompletionRequest,
        path: std::path::PathBuf,
        budget: BatchOutputBudget,
    }

    /// 首次成功 prepare：真实写盘 + 登记 + canonical 补登记。
    fn prepare_once(case: &str) -> Prepared {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, case, 1);
        let value = json!({"output": "d".repeat(8_000)});
        let request = request_with(tool_message(
            case,
            vec![ToolResultContent::json(value)],
            false,
        ));
        let budget = budget(dir.path(), run, tracker.clone(), 4096);
        let FitOutcome::Patched(_) = budget.fit_request(&request).unwrap() else {
            panic!("the first prepare must externalize");
        };
        let reference = tracker.calls()[0]
            .output_references
            .first()
            .cloned()
            .expect("supplemented reference");
        let path = dir
            .path()
            .join(".mio/outputs")
            .join(reference.path.strip_prefix(".mio/outputs/").unwrap());
        assert!(path.is_file());
        Prepared {
            dir,
            run,
            tracker,
            request,
            path,
            budget,
        }
    }

    /// 复用索引/缓存条目必须在捕获租约下证明磁盘真实普通文件的长度与 SHA-256：
    /// 删除、同长度改写、符号链接替换都必须在发布引用之前 fail-closed。
    #[test]
    fn reuse_verifies_real_disk_bytes_after_the_first_prepare() {
        // Deletion: a fresh writer (empty cache) must consult the index, then the
        // disk, and refuse instead of publishing a stale reference.
        let prepared = prepare_once("call-deleted");
        let before = prepared.tracker.calls()[0].output_references.clone();
        std::fs::remove_file(&prepared.path).unwrap();
        let fresh = budget(
            prepared.dir.path(),
            prepared.run,
            prepared.tracker.clone(),
            4096,
        );
        assert!(matches!(
            fresh.fit_request(&prepared.request),
            Err(FitError::Message(_))
        ));
        assert!(
            !prepared.path.exists(),
            "a deleted file must not be silently re-created"
        );
        assert_eq!(
            prepared.tracker.calls()[0].output_references,
            before,
            "a refused prepare adds no reference"
        );

        // Same-length corruption: same byte count, different content.
        let prepared = prepare_once("call-corrupt");
        let original = std::fs::read(&prepared.path).unwrap();
        let mut corrupted = original.clone();
        corrupted[0] ^= 0xFF;
        assert_eq!(corrupted.len(), original.len());
        std::fs::write(&prepared.path, &corrupted).unwrap();
        let fresh = budget(
            prepared.dir.path(),
            prepared.run,
            prepared.tracker.clone(),
            4096,
        );
        assert!(matches!(
            fresh.fit_request(&prepared.request),
            Err(FitError::Message(_))
        ));
        assert_eq!(
            std::fs::read(&prepared.path).unwrap(),
            corrupted,
            "an unknown existing file is never overwritten"
        );

        // Symlink replacement through the in-memory cache path.
        let prepared = prepare_once("call-symlink");
        let target = prepared.dir.path().join("elsewhere");
        std::fs::write(&target, b"other bytes").unwrap();
        std::fs::remove_file(&prepared.path).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &prepared.path).unwrap();
        assert!(matches!(
            prepared.budget.fit_request(&prepared.request),
            Err(FitError::Message(_))
        ));
        assert!(
            std::fs::symlink_metadata(&prepared.path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink is left in place, not followed or replaced"
        );
    }

    /// 整批不是每条固定上限：24 条正文在“最小引用元数据能放下”时必须成功，并按
    /// 剩余空间分配一个统一的 UTF-8 安全预览上限，而不是无谓 Stop。
    #[test]
    fn high_fanout_shares_the_remaining_preview_space_instead_of_stopping() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        let calls = 24usize;
        let mut contents = Vec::new();
        for index in 0..calls {
            register_call(&tracker, &format!("fan-{index}"), index as u64 + 1);
            contents.push(UserContent::ToolResult(
                call(&format!("fan-{index}")).result(vec![ToolResultContent::json(json!({
                    "index": index,
                    "output": "f".repeat(4_000),
                }))]),
            ));
        }
        let request = request_with(vec![
            Message::Assistant(mio_core::message::AssistantMessage::new(Vec::new())),
            Message::User { content: contents },
        ]);
        let limit = 16_000usize;
        let budget = budget(dir.path(), run, tracker.clone(), limit);
        let candidates = budget.collect_candidates(&request.chat_history);
        assert_eq!(candidates.len(), calls);
        let selected = beneficial_candidates(&candidates);
        assert_eq!(selected.len(), calls);
        let references = provisional_plan(&candidates, &selected);
        let metadata_only =
            patched_request_with_cap(&request, &candidates, &references, 0).unwrap();
        assert!(
            budget.request_cost(&metadata_only).unwrap() <= limit,
            "the minimal reference metadata must fit"
        );
        let per_item_preview =
            patched_request_with_cap(&request, &candidates, &references, PREVIEW_BYTES).unwrap();
        assert!(
            budget.request_cost(&per_item_preview).unwrap() > limit,
            "a fixed 2 KiB preview per result cannot fit"
        );
        let FitOutcome::Patched(patched) = budget.fit_request(&request).unwrap() else {
            panic!("metadata fits, so the whole batch must be externalized");
        };
        assert!(budget.request_cost(&patched).unwrap() <= limit);
        let Message::User { content } = &patched.chat_history[1] else {
            panic!("batch position");
        };
        let mut previews = Vec::new();
        for (index, item) in content.iter().enumerate().take(calls) {
            let UserContent::ToolResult(result) = item else {
                panic!("tool result {index}");
            };
            let ToolResultContent::Json { value } = &result.content[0] else {
                panic!("json result {index}");
            };
            assert_eq!(value["index"], json!(index), "order is preserved");
            assert!(value["output_references"].is_array());
            let preview = value["output"].as_str().unwrap();
            assert!(preview.len() <= PREVIEW_BYTES);
            previews.push(preview.len());
        }
        assert!(
            previews.iter().all(|&len| len > 0),
            "the remaining room is distributed as real preview, not dropped: {previews:?}"
        );
        assert!(
            previews.iter().all(|&len| len == previews[0]),
            "one shared UTF-8 safe cap for the whole batch: {previews:?}"
        );
    }

    /// 8 MiB 单文件硬上限在任何 I/O 之前校验：超限正文不进入候选，也不创建 `.mio`。
    #[test]
    fn body_above_the_eight_mib_hard_cap_is_rejected_before_io() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "call-huge", 1);
        let text = "h".repeat(MAX_OUTPUT_BYTES as usize + 1);
        let request = request_with(tool_message(
            "call-huge",
            vec![ToolResultContent::text(text)],
            false,
        ));
        let budget = budget(dir.path(), run, tracker, 4096);
        assert!(matches!(
            budget.fit_request(&request),
            Err(FitError::Irreducible { .. })
        ));
        assert!(
            !dir.path().join(".mio").exists(),
            "the hard cap is checked before any I/O"
        );
    }

    /// 元数据比正文更大的替换没有收益，不能选入计划；剩余空间仍不够时在零 I/O 下拒绝。
    #[test]
    fn non_beneficial_replacements_are_skipped_without_io() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "call-small", 1);
        let mut messages = tool_message(
            "call-small",
            vec![ToolResultContent::text("s".repeat(64))],
            false,
        );
        messages.push(Message::user("y".repeat(20_000)));
        let request = request_with(messages);
        let budget = budget(dir.path(), run, tracker.clone(), 4096);
        let candidates = budget.collect_candidates(&request.chat_history);
        assert_eq!(candidates.len(), 1, "the tiny body is still a candidate");
        assert!(
            beneficial_candidates(&candidates).is_empty(),
            "a replacement bigger than the body is not beneficial"
        );
        assert!(matches!(
            budget.fit_request(&request),
            Err(FitError::Irreducible { .. })
        ));
        assert!(!dir.path().join(".mio").exists());
        assert!(
            tracker.calls()[0].output_references.is_empty(),
            "nothing was externalized"
        );
    }

    /// 候选上限是有界算法限制，不是任意请求的数学覆盖声明。
    #[test]
    fn candidate_bound_is_a_bounded_limit_not_a_coverage_claim() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        let total = MAX_BATCH_CANDIDATES + 44;
        let mut contents = Vec::new();
        for index in 0..total {
            register_call(&tracker, &format!("cap-{index}"), index as u64 + 1);
            contents.push(UserContent::ToolResult(
                call(&format!("cap-{index}")).result(vec![ToolResultContent::json(json!({
                    "index": index,
                    "output": "c".repeat(3_000 + index),
                }))]),
            ));
        }
        let request = request_with(vec![
            Message::Assistant(mio_core::message::AssistantMessage::new(Vec::new())),
            Message::User { content: contents },
        ]);
        let budget = budget(dir.path(), run, tracker, 1);
        let candidates = budget.collect_candidates(&request.chat_history);
        assert_eq!(candidates.len(), MAX_BATCH_CANDIDATES);
        assert!(
            candidates[0].body.len() >= candidates[MAX_BATCH_CANDIDATES - 1].body.len(),
            "the bound keeps the largest bodies"
        );
    }

    /// 同一内容被两个调用复用时，每个调用的 canonical 记录保留自己的真实身份；
    /// 落盘索引条目的 `call_id` 始终是首个真实生产者，绝不改写成第二个调用。
    #[test]
    fn reused_content_keeps_each_calls_own_identity() {
        let dir = tempfile::tempdir().unwrap();
        let run = run_id();
        let tracker = tracker_for(run);
        register_call(&tracker, "id-a", 1);
        register_call(&tracker, "id-b", 2);
        let value = json!({"output": "i".repeat(8_000)});
        let mut messages =
            tool_message("id-a", vec![ToolResultContent::json(value.clone())], false);
        messages.extend(tool_message(
            "id-b",
            vec![ToolResultContent::json(value.clone())],
            false,
        ));
        let request = request_with(messages);
        let budget = budget(dir.path(), run, tracker.clone(), 8_000);
        let FitOutcome::Patched(_) = budget.fit_request(&request).unwrap() else {
            panic!("externalized");
        };
        let files = std::fs::read_dir(dir.path().join(".mio/outputs"))
            .unwrap()
            .count();
        assert_eq!(files, 1, "identical content is one content-addressed file");
        let records = tracker.calls();
        let a = records
            .iter()
            .find(|record| record.tool_call_id.as_deref() == Some("id-a"))
            .unwrap();
        let b = records
            .iter()
            .find(|record| record.tool_call_id.as_deref() == Some("id-b"))
            .unwrap();
        assert_eq!(a.output_references[0].call_id.as_deref(), Some("id-a"));
        assert_eq!(b.output_references[0].call_id.as_deref(), Some("id-b"));
        assert_eq!(a.output_references[0].path, b.output_references[0].path);
        let store = OutputStore::open(dir.path()).unwrap();
        let index = store.load_index().unwrap();
        assert_eq!(index.entries.len(), 1);
        assert_eq!(
            index.entries[0].call_id.as_deref(),
            Some("id-a"),
            "the index keeps the first real producer and never claims the second"
        );
        assert_eq!(
            index.entries[0].hash.as_deref(),
            Some(sha256_hex(&serde_json::to_vec(&value).unwrap()).as_str())
        );
    }

    /// 新建文件在登记失败时就地回滚：写盘成功但登记失败不会留下未登记孤儿。
    #[test]
    fn capture_transaction_rolls_back_a_fresh_file_when_registration_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutputStore::open(dir.path()).unwrap();
        let mut transaction = CaptureTransaction::begin(&store).unwrap();
        let sha256 = sha256_hex(b"fresh body");
        let name = format!("{sha256}.tool");
        let relative = format!(".mio/outputs/{name}");
        let path = store.output_dir().join(&name);
        transaction.enroll_fresh(relative.clone(), sha256.clone());
        assert!(write_published(&path, b"fresh body").unwrap());
        let wrong = OutputEntry {
            path: relative,
            bytes: Some(10),
            hash: Some(sha256_hex(b"different")),
            stream: "tool".into(),
            created: 0,
            call_id: None,
        };
        assert!(transaction.register(wrong).is_err());
        assert!(
            !path.exists(),
            "a fresh file that never registered must not leak"
        );
        assert!(store.load_index().unwrap().entries.is_empty());
    }

    /// 回滚绝不删除已经被索引认领的内容寻址文件。
    #[test]
    fn rollback_never_deletes_a_file_claimed_by_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let store = OutputStore::open(dir.path()).unwrap();
        let body = b"shared body";
        let sha256 = sha256_hex(body);
        let name = format!("{sha256}.tool");
        let relative = format!(".mio/outputs/{name}");
        let path = store.output_dir().join(&name);
        let lease = store.begin_capture().unwrap();
        assert!(write_published(&path, body).unwrap());
        let entry = OutputEntry {
            path: relative.clone(),
            bytes: Some(body.len() as u64),
            hash: Some(sha256.clone()),
            stream: "tool".into(),
            created: 0,
            call_id: None,
        };
        store.register(&lease, entry).unwrap();
        drop(lease);
        let mut transaction = CaptureTransaction::begin(&store).unwrap();
        transaction.enroll_fresh(relative, sha256);
        drop(transaction);
        assert!(
            path.exists(),
            "a registered content-addressed file is never deleted by another transaction"
        );
        assert_eq!(store.load_index().unwrap().entries.len(), 1);
    }
}
