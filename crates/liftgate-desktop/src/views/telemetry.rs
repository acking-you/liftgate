//! Tab 02: Live SSE Telemetry, Warm Session Pool & Turn Inspector (`实时监控与会话`).

use gpui_kit::base::Disableable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::Input;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{IconName, Sizable as _};
use gpui_kit::*;
use liftgate_core::format::{format_latency_ms, format_relative_ms, format_tokens};
use liftgate_types::telemetry::{SessionSlotState, TaskPhase, TaskProtocol, TaskSnapshot};

use crate::app::{DesktopApp, TaskProtocolFilter};
use crate::i18n::tr;
use crate::theme::{
    BadgeTone, Palette, editorial_card, kpi_card, kv_row, pill_badge, section_header,
    sf_terminal_box,
};

impl DesktopApp {
    pub(crate) fn render_telemetry_tab(&self, p: &Palette, cx: &mut Context<Self>) -> Div {
        let lang = self.lang;
        let Some(snap) = self.live_telemetry.as_ref().or_else(|| {
            self.gateway_report
                .as_ref()
                .and_then(|r| r.telemetry.as_ref())
        }) else {
            return editorial_card(p)
                .items_start()
                .child(section_header(
                    p,
                    "TELEMETRY OFFLINE",
                    tr(lang, "实时遥测尚未连接", "Live Telemetry Not Connected"),
                    Some(tr(
                        lang,
                        "本地网关当前未运行或不可达。启动网关后将自动建立 `/api/telemetry/events` SSE 实时推送流。",
                        "Gateway is currently offline. Start the daemon to stream live `/api/telemetry/events` snapshots.",
                    )),
                ))
                .child(
                    Button::new("tel-start-gw")
                        .small()
                        .primary()
                        .loading(self.busy_gateway)
                        .icon(IconName::Play)
                        .label(tr(lang, "立即启动本地网关", "Start Local Gateway"))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.action_start_gateway(false, window, cx);
                        })),
                );
        };

        let now_ms = snap.generated_at_unix_ms;
        let acp = &snap.acp_process;
        let tp = &snap.metrics;
        let sp = &snap.session_pool;

        // --- ACP Process & SSE Status Strip ---
        let acp_strip = editorial_card(p).child(
            div()
                .w_full()
                .flex()
                .flex_wrap()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(if self.sse_connected {
                            pill_badge(p, BadgeTone::Ok, "SSE STREAM CONNECTED")
                        } else {
                            pill_badge(p, BadgeTone::Warn, "SNAPSHOT POLLING")
                        })
                        .child(if acp.connected {
                            pill_badge(p, BadgeTone::Ok, "ACP CONNECTED")
                        } else {
                            pill_badge(p, BadgeTone::Neutral, "ACP IDLE / LAZY")
                        })
                        .child(pill_badge(
                            p,
                            BadgeTone::Brand,
                            format!("Default: {}", acp.default_model),
                        ))
                        .child(pill_badge(
                            p,
                            BadgeTone::Neutral,
                            format!("Mode: {}", acp.acp_mode),
                        )),
                )
                .child(div().text_xs().text_color(p.muted).child(format!(
                    "JSON-RPC req {} · resp {} · notif {} · pending {} · active streams {}",
                    acp.rpc_requests_sent,
                    acp.rpc_responses_received,
                    acp.rpc_notifications_received,
                    acp.rpc_pending_in_flight,
                    acp.active_prompt_streams
                ))),
        );

        // --- Token Breakdown Row ---
        let total_tok = tp.total_input_tokens + tp.total_output_tokens + tp.total_thought_tokens;
        let token_row = div()
            .w_full()
            .flex()
            .flex_row()
            .gap_3()
            .child(kpi_card(
                p,
                tr(lang, "输入 Tokens", "INPUT TOKENS"),
                format_tokens(tp.total_input_tokens),
                format!("Completed turns: {}", tp.completed_requests),
            ))
            .child(kpi_card(
                p,
                tr(lang, "输出 Tokens", "OUTPUT TOKENS"),
                format_tokens(tp.total_output_tokens),
                format!(
                    "Cancelled: {} · Failed: {}",
                    tp.cancelled_requests, tp.failed_requests
                ),
            ))
            .child(kpi_card(
                p,
                tr(lang, "缓存命中读取", "CACHED READ TOKENS"),
                format_tokens(tp.total_cached_tokens),
                tr(
                    lang,
                    "热会话前缀复用节省开销",
                    "Saved via warm ACP prefix reuse",
                ),
            ))
            .child(kpi_card(
                p,
                tr(lang, "深度思考 Tokens", "THOUGHT TOKENS"),
                format_tokens(tp.total_thought_tokens),
                format!("Total: {}", format_tokens(total_tok)),
            ));

        // --- Warm Session Pool Card ---
        let sessions_card = editorial_card(p)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(section_header(
                        p,
                        "BOUNDED LRU SESSION POOL",
                        tr(lang, "驻留热会话池管理", "Warm ACP Session Pool"),
                        None,
                    ))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(pill_badge(
                                p,
                                BadgeTone::Ok,
                                format!("Active {}", sp.active_leased_sessions),
                            ))
                            .child(pill_badge(
                                p,
                                BadgeTone::Info,
                                format!("Idle {} / {}", sp.idle_cached_sessions, sp.max_idle_sessions),
                            ))
                            .child(pill_badge(
                                p,
                                BadgeTone::Neutral,
                                format!(
                                    "Created {} · Reused {} · Evicted {}",
                                    sp.total_sessions_created,
                                    sp.total_sessions_reused,
                                    sp.total_sessions_closed_or_evicted
                                ),
                            )),
                    ),
            )
            .child(if sp.sessions.is_empty() {
                div()
                    .py_3()
                    .text_xs()
                    .text_color(p.muted)
                    .child(tr(
                        lang,
                        "当前暂无驻留热会话。发起任意 Claude / Codex 请求后，热会话将自动驻留在此处复用。",
                        "No cached sessions yet. Send a Claude or Codex turn to populate warm sessions.",
                    ))
            } else {
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .children(sp.sessions.iter().enumerate().map(|(idx, sess)| {
                        let is_running = matches!(sess.state, SessionSlotState::Running);
                        let sid_copy = sess.session_id.clone();
                        let sid_evict = sess.session_id.clone();
                        let short_sid: String = sess.session_id.chars().take(18).collect();
                        div()
                            .w_full()
                            .px_3()
                            .py_2()
                            .rounded(px(8.0))
                            .bg(p.surface_alt)
                            .border_1()
                            .border_color(p.border)
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap_2()
                                    .child(if is_running {
                                        pill_badge(p, BadgeTone::Brand, "RUNNING")
                                    } else {
                                        pill_badge(p, BadgeTone::Ok, "IDLE · WARM")
                                    })
                                    .child(
                                        div()
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(p.text)
                                            .child(sess.model_id.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(p.muted)
                                            .child(format!("sid: {short_sid}…")),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(p.text_2)
                                            .child(format!(
                                                "{} turns · {} msgs · active {}",
                                                sess.turn_count,
                                                sess.message_count,
                                                format_relative_ms(sess.last_active_unix_ms, now_ms)
                                            )),
                                    ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .child(
                                        Button::new(SharedString::from(format!("cp-sid-{idx}")))
                                            .xsmall()
                                            .ghost()
                                            .icon(IconName::Copy)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.copy_text(
                                                    "Session ID",
                                                    sid_copy.clone(),
                                                    cx,
                                                );
                                            })),
                                    )
                                    .child(
                                        Button::new(SharedString::from(format!("evict-sid-{idx}")))
                                            .xsmall()
                                            .danger()
                                            .disabled(is_running)
                                            .icon(IconName::Delete)
                                            .label(tr(lang, "驱逐", "Evict"))
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.action_evict_session(sid_evict.clone(), cx);
                                            })),
                                    ),
                            )
                    }))
            });

        // --- Master-Detail Tasks & Turn Inspector ---
        let query = self
            .in_task_filter
            .read(cx)
            .value()
            .trim()
            .to_ascii_lowercase();
        let proto_filter = self.protocol_filter;
        let mut combined_tasks: Vec<(&TaskSnapshot, bool)> = Vec::new();
        for t in &snap.active_tasks {
            if proto_filter.matches(t.protocol) && task_matches_query(t, &query) {
                combined_tasks.push((t, true));
            }
        }
        for t in &snap.recent_tasks {
            if proto_filter.matches(t.protocol) && task_matches_query(t, &query) {
                combined_tasks.push((t, false));
            }
        }

        let selected_task: Option<&TaskSnapshot> = self
            .selected_task_id
            .as_deref()
            .and_then(|id| {
                combined_tasks
                    .iter()
                    .find(|(t, _)| t.task_id == id)
                    .map(|(t, _)| *t)
            })
            .or_else(|| combined_tasks.first().map(|(t, _)| *t));

        let filter_bar = div()
            .w_full()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap_3()
            .child(
                div()
                    .flex_1()
                    .min_w(px(220.0))
                    .child(Input::new(&self.in_task_filter).small().cleanable(true)),
            )
            .child(div().flex().items_center().gap_1p5().children(
                TaskProtocolFilter::ALL.into_iter().map(|pf| {
                    let active = self.protocol_filter == pf;
                    let btn = Button::new(SharedString::from(format!("pf-{pf:?}")))
                        .xsmall()
                        .label(pf.label(lang));
                    let styled = if active { btn.primary() } else { btn.outline() };
                    styled.on_click(cx.listener(move |this, _, _, cx| {
                        this.protocol_filter = pf;
                        cx.notify();
                    }))
                }),
            ));

        let list_pane = div()
            .id("telemetry-task-list")
            .w(px(430.0))
            .max_h(px(480.0))
            .pr_1()
            .flex()
            .flex_col()
            .gap_2()
            .children(if combined_tasks.is_empty() {
                vec![
                    div()
                        .p_4()
                        .rounded(px(8.0))
                        .bg(p.surface_alt)
                        .text_xs()
                        .text_color(p.muted)
                        .child(tr(
                            lang,
                            "暂无匹配的请求轮次记录。",
                            "No matching API turns yet.",
                        ))
                        .into_any_element(),
                ]
            } else {
                combined_tasks
                    .iter()
                    .enumerate()
                    .map(|(idx, (task, is_live))| {
                        let is_sel = selected_task.is_some_and(|s| s.task_id == task.task_id);
                        let tid = task.task_id.clone();
                        let (phase_lbl, phase_tone) = phase_badge_meta(task.phase);
                        let row_bg = if is_sel {
                            p.primary_soft
                        } else if *is_live {
                            p.warn_soft
                        } else {
                            p.surface_alt
                        };
                        let row_border = if is_sel { p.primary } else { p.border };
                        div()
                            .id(SharedString::from(format!("task-row-{idx}")))
                            .cursor_pointer()
                            .w_full()
                            .p_2p5()
                            .rounded(px(8.0))
                            .bg(row_bg)
                            .border_1()
                            .border_color(row_border)
                            .flex()
                            .flex_col()
                            .gap_1()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.selected_task_id = Some(tid.clone());
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w_full()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(
                                        div()
                                            .flex()
                                            .items_center()
                                            .gap_1p5()
                                            .child(pill_badge(p, phase_tone, phase_lbl))
                                            .children(
                                                task.session_reused
                                                    .then(|| pill_badge(p, BadgeTone::Ok, "WARM")),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(p.muted)
                                            .child(format_latency_ms(task.elapsed_ms)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(p.text)
                                    .child(task.resolved_model.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(p.text_2)
                                    .child(truncate_preview(&task.prompt_preview, 68)),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<AnyElement>>()
            })
            .overflow_y_scrollbar();

        let detail_pane = if let Some(t) = selected_task {
            let (phase_lbl, phase_tone) = phase_badge_meta(t.phase);
            let proto_name = match t.protocol {
                TaskProtocol::OpenaiResponses => "OpenAI /v1/responses",
                TaskProtocol::AnthropicMessages => "Anthropic /v1/messages",
            };
            let prompt_copy = t.prompt_preview.clone();
            let out_copy = t.output_preview.clone();
            let tid_copy = t.task_id.clone();
            let usage_line = t.usage.as_ref().map_or_else(
                || "—".to_owned(),
                |u| {
                    format!(
                        "in {} · out {} · cache {} · thought {} · total {}",
                        format_tokens(u.input_tokens),
                        format_tokens(u.output_tokens),
                        format_tokens(u.cached_read_tokens),
                        format_tokens(u.thought_tokens),
                        format_tokens(u.total_tokens)
                    )
                },
            );

            div()
                .flex_1()
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(pill_badge(p, phase_tone, phase_lbl))
                                .child(pill_badge(p, BadgeTone::Info, proto_name))
                                .child(if t.session_reused {
                                    pill_badge(p, BadgeTone::Ok, "WARM SESSION REUSED")
                                } else {
                                    pill_badge(p, BadgeTone::Neutral, "COLD NEW SESSION")
                                }),
                        )
                        .child(
                            Button::new("copy-task-id")
                                .xsmall()
                                .ghost()
                                .icon(IconName::Copy)
                                .label(tr(lang, "复制 Task ID", "Copy Task ID"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_text("Task ID", tid_copy.clone(), cx);
                                })),
                        ),
                )
                .children(t.error_message.as_ref().map(|err_msg| {
                    div()
                        .w_full()
                        .p_2p5()
                        .rounded(px(8.0))
                        .bg(p.err_soft)
                        .border_1()
                        .border_color(p.err.opacity(0.45))
                        .text_xs()
                        .text_color(p.err)
                        .child(format!("Error: {err_msg}"))
                }))
                .child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded(px(8.0))
                        .bg(p.surface_alt)
                        .border_1()
                        .border_color(p.border)
                        .flex()
                        .flex_col()
                        .child(kv_row(
                            p,
                            tr(lang, "请求 / 实际模型", "Requested -> Resolved"),
                            format!("{} -> {}", t.requested_model, t.resolved_model),
                        ))
                        .child(kv_row(
                            p,
                            "Session ID",
                            t.session_id.clone().unwrap_or_else(|| "—".to_owned()),
                        ))
                        .child(kv_row(
                            p,
                            tr(lang, "耗时与结束原因", "Elapsed / Stop Reason"),
                            format!(
                                "{} · stream={} · stop={}",
                                format_latency_ms(t.elapsed_ms),
                                t.stream,
                                t.stop_reason.as_deref().unwrap_or("in_progress")
                            ),
                        ))
                        .child(kv_row(
                            p,
                            tr(lang, "工具调用与联网检索", "Tools & Native Web"),
                            format!(
                                "client_tools={} · web_search={} · web_fetch={}",
                                if t.client_tool_calls.is_empty() {
                                    "0".to_owned()
                                } else {
                                    format!(
                                        "{} ({})",
                                        t.client_tool_calls.len(),
                                        t.client_tool_calls.join(", ")
                                    )
                                },
                                t.native_web_searches,
                                t.native_web_fetches
                            ),
                        ))
                        .child(kv_row(p, "Token Usage", usage_line)),
                )
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.text_2)
                                .child(tr(lang, "输入 Prompt 预览", "Prompt Preview")),
                        )
                        .child(
                            Button::new("copy-prompt-prev")
                                .xsmall()
                                .ghost()
                                .icon(IconName::Copy)
                                .label(tr(lang, "复制", "Copy"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_text("Prompt Preview", prompt_copy.clone(), cx);
                                })),
                        ),
                )
                .child(sf_terminal_box(
                    p,
                    "task-prompt-box",
                    "prompt_preview",
                    if t.prompt_preview.is_empty() {
                        "(empty)"
                    } else {
                        &t.prompt_preview
                    },
                    px(130.0),
                ))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(p.text_2)
                                .child(format!(
                                    "{} (thought {} chars · output {} chars)",
                                    tr(lang, "输出响应预览", "Output Preview"),
                                    t.thought_chars,
                                    t.output_chars
                                )),
                        )
                        .child(
                            Button::new("copy-out-prev")
                                .xsmall()
                                .ghost()
                                .icon(IconName::Copy)
                                .label(tr(lang, "复制", "Copy"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.copy_text("Output Preview", out_copy.clone(), cx);
                                })),
                        ),
                )
                .child(sf_terminal_box(
                    p,
                    "task-output-box",
                    "output_preview",
                    if t.output_preview.is_empty() {
                        "(streaming or empty)"
                    } else {
                        &t.output_preview
                    },
                    px(160.0),
                ))
        } else {
            div()
                .flex_1()
                .p_6()
                .rounded(px(8.0))
                .bg(p.surface_alt)
                .text_xs()
                .text_color(p.muted)
                .child(tr(
                    lang,
                    "在左侧选择一条请求轮次即可查看完整协议元数据、工具调用、Prompt 与流式响应预览。",
                    "Select a turn on the left to inspect full protocol metadata, tool calls, prompt and output previews.",
                ))
        };

        let inspector_card = editorial_card(p)
            .child(section_header(
                p,
                "LIVE & HISTORICAL TURN INSPECTOR",
                tr(
                    lang,
                    "API 请求追踪与深度检视",
                    "Turn Stream & Deep Inspector",
                ),
                None,
            ))
            .child(filter_bar)
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap_4()
                    .child(list_pane)
                    .child(detail_pane),
            );

        div()
            .w_full()
            .flex()
            .flex_col()
            .gap_4()
            .child(acp_strip)
            .child(token_row)
            .child(sessions_card)
            .child(inspector_card)
    }
}

const fn phase_badge_meta(phase: TaskPhase) -> (&'static str, BadgeTone) {
    match phase {
        TaskPhase::InitializingSession => ("INIT SESSION", BadgeTone::Info),
        TaskPhase::Thinking => ("THINKING", BadgeTone::Warn),
        TaskPhase::SearchingWeb => ("WEB SEARCH", BadgeTone::Brand),
        TaskPhase::FetchingWeb => ("WEB FETCH", BadgeTone::Brand),
        TaskPhase::StreamingText => ("STREAMING", BadgeTone::Brand),
        TaskPhase::CallingClientTool => ("CLIENT TOOL", BadgeTone::Info),
        TaskPhase::Completed => ("COMPLETED", BadgeTone::Ok),
        TaskPhase::Failed => ("FAILED", BadgeTone::Err),
        TaskPhase::Cancelled => ("CANCELLED", BadgeTone::Neutral),
    }
}

fn task_matches_query(task: &TaskSnapshot, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    task.task_id.to_ascii_lowercase().contains(query)
        || task.requested_model.to_ascii_lowercase().contains(query)
        || task.resolved_model.to_ascii_lowercase().contains(query)
        || task
            .session_id
            .as_deref()
            .is_some_and(|s| s.to_ascii_lowercase().contains(query))
        || task.prompt_preview.to_ascii_lowercase().contains(query)
        || task.output_preview.to_ascii_lowercase().contains(query)
}

fn truncate_preview(text: &str, max_chars: usize) -> String {
    let single_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.is_empty() {
        return "(empty prompt)".to_owned();
    }
    let mut out = String::new();
    for (idx, ch) in single_line.chars().enumerate() {
        if idx >= max_chars {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}
