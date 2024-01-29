use editor::{Editor, EditorEvent};
use gpui::{
    AnyElement, AppContext, EventEmitter, FocusHandle, FocusableView, InteractiveElement,
    IntoElement, ParentElement, Render, Styled, View, ViewContext,
};
use language::LanguageRegistry;
use std::sync::Arc;
use ui::prelude::*;
use workspace::item::Item;
use workspace::Workspace;

use crate::{markdown_renderer::render_markdown, OpenPreview};

pub struct MarkdownPreviewView {
    focus_handle: FocusHandle,
    languages: Arc<LanguageRegistry>,
    contents: String,
}

impl MarkdownPreviewView {
    pub fn register(workspace: &mut Workspace, _cx: &mut ViewContext<Workspace>) {
        let languages = workspace.app_state().languages.clone();

        workspace.register_action(move |workspace, _: &OpenPreview, cx| {
            if workspace.has_active_modal(cx) {
                cx.propagate();
                return;
            }
            let languages = languages.clone();
            let editor = workspace.active_item_as::<Editor>(cx).unwrap();
            MarkdownPreviewView::deploy_preview(workspace, languages, editor, cx);
            cx.notify();
        });
    }

    pub fn new(
        active_editor: View<Editor>,
        languages: Arc<LanguageRegistry>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        cx.subscribe(&active_editor, |this, editor, event: &EditorEvent, cx| {
            if *event == EditorEvent::Edited {
                let editor = editor.read(cx);
                let contents = editor.buffer().read(cx).snapshot(cx).text();
                this.contents = contents;
                cx.notify();
            }
        })
        .detach();

        let editor = active_editor.read(cx);
        let contents = editor.buffer().read(cx).snapshot(cx).text();

        Self {
            focus_handle,
            languages,
            contents,
        }
    }

    // Re-active the most recently active preview, or create a new one if there isn't one.
    fn deploy_preview(
        workspace: &mut Workspace,
        languages: Arc<LanguageRegistry>,
        active_editor: View<Editor>,
        cx: &mut ViewContext<Workspace>,
    ) {
        let existing = workspace
            .active_pane()
            .read(cx)
            .items()
            .find_map(|item| item.downcast::<MarkdownPreviewView>());

        Self::existing_or_new_preview(workspace, languages, active_editor, existing, cx)
    }

    fn existing_or_new_preview(
        workspace: &mut Workspace,
        languages: Arc<LanguageRegistry>,
        active_editor: View<Editor>,
        existing: Option<View<MarkdownPreviewView>>,
        cx: &mut ViewContext<Workspace>,
    ) {
        if let Some(existing) = existing {
            workspace.activate_item(&existing, cx);
            existing
        } else {
            let view: View<MarkdownPreviewView> =
                cx.new_view(|cx| MarkdownPreviewView::new(active_editor, languages, cx));

            workspace.split_item(workspace::SplitDirection::Right, Box::new(view.clone()), cx);
            view
        };
    }
}

impl FocusableView for MarkdownPreviewView {
    fn focus_handle(&self, _: &AppContext) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewEvent {}

impl EventEmitter<PreviewEvent> for MarkdownPreviewView {}

impl Item for MarkdownPreviewView {
    type Event = PreviewEvent;

    fn tab_content(
        &self,
        _detail: Option<usize>,
        selected: bool,
        _cx: &WindowContext,
    ) -> AnyElement {
        h_flex()
            .gap_2()
            .child(Icon::new(IconName::FileDoc).color(if selected {
                Color::Default
            } else {
                Color::Muted
            }))
            .child(Label::new("Markdown preview").color(if selected {
                Color::Default
            } else {
                Color::Muted
            }))
            .into_any()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("markdown preview")
    }

    fn to_item_events(_event: &Self::Event, _f: impl FnMut(workspace::item::ItemEvent)) {}
}

impl Render for MarkdownPreviewView {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        // TODO: This is wrong but I'm unsure how
        // to make the preview scrollable without specifying
        // a real height as max_h_full does not work.
        let viewport_height = cx.viewport_size().height;

        let mut container = v_flex()
            .items_start()
            .justify_start()
            .key_context("MarkdownPreview")
            .track_focus(&self.focus_handle)
            .max_h(viewport_height)
            .id("MarkdownPreview")
            .overflow_scroll()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .p_4();

        // TODO: render_markdown() doesn't need to be called every time,
        // only when the contents change.
        // Unable to do this because `Div` doesn't implement `clone`.
        for item in render_markdown(&self.contents, cx, &self.languages).into_iter() {
            container = container.child(item.mb_2());
        }

        container.into_any()
    }
}
