use std::{ops::Range, sync::Arc};

use gpui::{
    div, px, rems, AbsoluteLength, AnyElement, DefiniteLength, Div, ElementId, Hsla, ParentElement,
    SharedString, Styled, StyledText, TextStyle, WindowContext,
};
use language::LanguageRegistry;
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};
use rich_text::render_rich_text;
use theme::ActiveTheme;
use ui::{h_flex, v_flex};

enum TableState {
    Header,
    Body,
}

struct MarkdownTable {
    header: Vec<Div>,
    body: Vec<Vec<Div>>,
    current_row: Vec<Div>,
    state: TableState,
    border_color: Hsla,
}

impl MarkdownTable {
    fn new(border_color: Hsla) -> Self {
        Self {
            header: Vec::new(),
            body: Vec::new(),
            current_row: Vec::new(),
            state: TableState::Header,
            border_color,
        }
    }

    fn finish_row(&mut self) {
        match self.state {
            TableState::Header => {
                self.header.extend(self.current_row.drain(..));
                self.state = TableState::Body;
            }
            TableState::Body => {
                self.body.push(self.current_row.drain(..).collect());
            }
        }
    }

    fn add_cell(&mut self, contents: AnyElement) {
        let cell = div()
            .child(contents)
            .w_full()
            .px_2()
            .py_1()
            .border_color(self.border_color);

        let cell = match self.state {
            TableState::Header => cell.border_2(),
            TableState::Body => cell.border_1(),
        };

        self.current_row.push(cell);
    }

    fn finish(self) -> Div {
        let mut table = v_flex().w_full();
        let mut header = h_flex();

        for cell in self.header {
            header = header.child(cell);
        }
        table = table.child(header);
        for row in self.body {
            let mut row_div = h_flex();
            for cell in row {
                row_div = row_div.child(cell);
            }
            table = table.child(row_div);
        }
        table
    }
}

struct Renderer<I> {
    source_contents: String,

    iter: I,

    finished: Vec<Div>,

    language_registry: Arc<LanguageRegistry>,

    table: Option<MarkdownTable>,
    list_depth: usize,
    block_quote_depth: usize,

    ui_text_color: Hsla,
    ui_text_muted_color: Hsla,
    ui_code_background: Hsla,
    ui_border_color: Hsla,
    ui_text_style: TextStyle,
}

impl<'a, I> Renderer<I>
where
    I: Iterator<Item = (Event<'a>, Range<usize>)>,
{
    fn new(
        iter: I,
        source_contents: String,
        language_registry: &Arc<LanguageRegistry>,
        ui_text_color: Hsla,
        ui_text_muted_color: Hsla,
        ui_code_background: Hsla,
        ui_border_color: Hsla,
        text_style: TextStyle,
    ) -> Self {
        Self {
            iter,
            source_contents,
            table: None,
            finished: vec![],
            language_registry: language_registry.clone(),
            list_depth: 0,
            block_quote_depth: 0,
            ui_border_color,
            ui_text_color,
            ui_text_muted_color,
            ui_code_background,
            ui_text_style: text_style,
        }
    }

    fn run(mut self) -> Self {
        while let Some((event, source_range)) = self.iter.next() {
            match event {
                Event::Start(tag) => {
                    self.start_tag(tag);
                }
                Event::End(tag) => {
                    self.end_tag(tag, source_range);
                }
                Event::Rule => {
                    let rule = div().w_full().h(px(2.)).bg(self.ui_border_color);
                    self.finished.push(div().mb_4().child(rule));
                }
                _ => {
                    // TODO: SoftBreak, HardBreak, FootnoteReference
                }
            }
        }
        self
    }

    fn render_md_from_range(&self, source_range: Range<usize>) -> gpui::AnyElement {
        let mentions = &[];
        let language = None;
        let paragraph = &self.source_contents[source_range.clone()];
        let rich_text = render_rich_text(
            paragraph.into(),
            mentions,
            &self.language_registry,
            language,
        );
        let id: ElementId = source_range.start.into();
        rich_text.element_no_cx(id, self.ui_text_style.clone(), self.ui_code_background)
    }

    fn start_tag(&mut self, tag: Tag<'a>) {
        match tag {
            Tag::List(_) => {
                self.list_depth += 1;
            }
            Tag::BlockQuote => {
                self.block_quote_depth += 1;
            }
            Tag::Table(_text_alignments) => {
                self.table = Some(MarkdownTable::new(self.ui_border_color));
            }
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: Tag, source_range: Range<usize>) {
        match tag {
            Tag::Paragraph => {
                if self.list_depth > 0 || self.block_quote_depth > 0 {
                    return;
                }

                let element = self.render_md_from_range(source_range.clone());
                let paragraph = h_flex().mb_3().child(element);

                self.finished.push(paragraph);
            }
            Tag::Heading(level, _, _) => {
                let mut headline = self.headline(level);
                if source_range.start > 0 {
                    headline = headline.mt_4();
                }

                let element = self.render_md_from_range(source_range.clone());
                let headline = headline.child(element);

                self.finished.push(headline);
            }
            Tag::List(_) => {
                if self.list_depth == 1 {
                    let element = self.render_md_from_range(source_range.clone());
                    let list = div().mb_3().child(element);

                    self.finished.push(list);
                }

                self.list_depth -= 1;
            }
            Tag::BlockQuote => {
                let element = self.render_md_from_range(source_range.clone());

                let block_quote = h_flex()
                    .mb_3()
                    .child(
                        div()
                            .w(px(4.))
                            .bg(self.ui_border_color)
                            .h_full()
                            .mr_2()
                            .mt_1(),
                    )
                    .text_color(self.ui_text_muted_color)
                    .child(element);

                self.finished.push(block_quote);

                self.block_quote_depth -= 1;
            }
            Tag::CodeBlock(kind) => {
                let contents = self.source_contents[source_range.clone()].trim();
                let contents = contents.trim_start_matches("```");
                let contents = contents.trim_end_matches("```");
                let contents = match kind {
                    CodeBlockKind::Fenced(language) => {
                        contents.trim_start_matches(&language.to_string())
                    }
                    CodeBlockKind::Indented => contents,
                };
                let contents: String = contents.into();
                let contents = SharedString::from(contents);

                let code_block = div()
                    .mb_3()
                    .px_4()
                    .py_0()
                    .bg(self.ui_code_background)
                    .child(StyledText::new(contents));

                self.finished.push(code_block);
            }
            Tag::Table(_alignment) => {
                if self.table.is_none() {
                    log::error!("Table end without table ({:?})", source_range);
                    return;
                }

                let table = self.table.take().unwrap();
                let table = table.finish().mb_4();
                self.finished.push(table);
            }
            Tag::TableHead => {
                if self.table.is_none() {
                    log::error!("Table head without table ({:?})", source_range);
                    return;
                }

                self.table.as_mut().unwrap().finish_row();
            }
            Tag::TableRow => {
                if self.table.is_none() {
                    log::error!("Table row without table ({:?})", source_range);
                    return;
                }

                self.table.as_mut().unwrap().finish_row();
            }
            Tag::TableCell => {
                if self.table.is_none() {
                    log::error!("Table cell without table ({:?})", source_range);
                    return;
                }

                let contents = self.render_md_from_range(source_range.clone());
                self.table.as_mut().unwrap().add_cell(contents);
            }
            _ => {}
        }
    }

    fn headline(&self, level: HeadingLevel) -> Div {
        let size = match level {
            HeadingLevel::H1 => rems(2.),
            HeadingLevel::H2 => rems(1.5),
            HeadingLevel::H3 => rems(1.25),
            HeadingLevel::H4 => rems(1.),
            HeadingLevel::H5 => rems(0.875),
            HeadingLevel::H6 => rems(0.85),
        };

        let line_height = DefiniteLength::Absolute(AbsoluteLength::Rems(rems(1.25)));

        let color = match level {
            HeadingLevel::H6 => self.ui_text_muted_color,
            _ => self.ui_text_color,
        };

        let headline = h_flex()
            .w_full()
            .line_height(line_height)
            .text_size(size)
            .text_color(color)
            .mb_4()
            .pb(rems(0.15));

        headline
    }
}

pub fn render_markdown(
    markdown_input: &str,
    cx: &WindowContext,
    language_registry: &Arc<LanguageRegistry>,
) -> Vec<Div> {
    // TODO: Move all of this to the renderer
    let theme = cx.theme();
    let ui_code_background = theme.colors().surface_background;
    let ui_text_color = theme.colors().text;
    let ui_text_muted_color = theme.colors().text_muted;
    let ui_border_color = theme.colors().border;
    let text_style = cx.text_style();

    let options = Options::all();
    let parser = Parser::new_ext(markdown_input, options);
    let renderer = Renderer::new(
        parser.into_offset_iter(),
        markdown_input.to_owned(),
        language_registry,
        ui_text_color,
        ui_text_muted_color,
        ui_code_background,
        ui_border_color,
        text_style,
    );
    let renderer = renderer.run();
    return renderer.finished;
}
