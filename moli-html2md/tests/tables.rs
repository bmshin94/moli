mod support;

use moli_html2md::{Converter, Options, convert};
use support::{Tree, rendered_html};

fn markdown(html: &str) -> String {
    let dom = Tree::parse(html);
    let before = dom.clone();
    let result = convert(&dom, dom.root);
    assert_eq!(dom, before);
    result
}

#[test]
fn hacker_news_layout_tables_preserve_story_and_metadata_order() {
    let result = markdown(include_str!("fixtures/hacker-news-layout.html"));
    assert_eq!(
        result,
        include_str!("fixtures/hacker-news-layout.md").trim_end()
    );
    let html = rendered_html(&result);
    // The simple navigation row becomes a table; nested/spanning story layout
    // still expands into blocks in the original content order.
    assert_eq!(html.matches("<table>").count(), 1);
    assert!(!html.contains("<br>"));
    assert_eq!(html.matches("<a ").count(), 14);
}

#[test]
fn nested_data_table_survives_an_expanded_layout_table() {
    let html = "<table><tr><td>before</td></tr><tr><td><table><tr><th>Key</th><th>Value</th></tr><tr><td>a|b</td><td><code>c|d</code></td></tr></table></td></tr><tr><td>after</td></tr></table>";
    let actual = markdown(html);
    assert_eq!(
        actual,
        "before\n\n| Key | Value |\n| --- | --- |\n| a\\|b | `c\\|d` |\n\nafter"
    );
    assert_eq!(
        rendered_html(&actual),
        "<p>before</p>\n<table><thead><tr><th>Key</th><th>Value</th></tr></thead><tbody>\n<tr><td>a|b</td><td><code>c|d</code></td></tr>\n</tbody></table>\n<p>after</p>\n"
    );
}

#[test]
fn wrapping_a_table_preserves_its_content() {
    let data =
        "<table><tr><td>one</td><td>two</td></tr><tr><td>three</td><td>four</td></tr></table>";
    assert_eq!(
        markdown(&format!("<table><tr><td>{data}</td></tr></table>")),
        markdown(data)
    );
    assert_eq!(
        markdown(data),
        "|  |  |\n| --- | --- |\n| one | two |\n| three | four |"
    );
}

#[test]
fn table_roles_do_not_change_conversion() {
    for role in [
        "",
        "presentation",
        "none",
        " PRESENTATION ",
        "table",
        "grid",
    ] {
        let html = format!(
            "<table role='{role}'><tr><th>Name</th><th>Value</th></tr><tr><td>one</td><td>two</td></tr></table>"
        );
        assert_eq!(
            markdown(&html),
            "| Name | Value |\n| --- | --- |\n| one | two |"
        );
    }
}

#[test]
fn table_cells_preserve_block_syntax_and_code_whitespace() {
    let html = "<table><tr><td>intro</td><td><h2>Heading</h2><pre><code>  a\n  b\n</code></pre><blockquote>quote</blockquote><ul><li>item</li></ul></td><td>end</td></tr></table>";
    assert_eq!(
        markdown(html),
        "intro\n\n## Heading\n\n```\n  a\n  b\n```\n\n> quote\n\n- item\n\nend"
    );
}

#[test]
fn captions_sections_headers_and_spacer_rows_preserve_content_order() {
    for (html, expected) in [
        (
            "<table role='table'><tr><td>a</td><td>b</td></tr><tr></tr><tr><td>c</td><td>d</td></tr></table>",
            "|  |  |\n| --- | --- |\n| a | b |\n| c | d |",
        ),
        (
            "<table><caption>Data</caption><tr><td>a</td><td>b</td></tr><tr></tr><tr><td>c</td><td>d</td></tr></table>",
            "Data\n\n|  |  |\n| --- | --- |\n| a | b |\n| c | d |",
        ),
        (
            "<table><thead><tr><td>a</td><td>b</td></tr></thead><tbody><tr></tr><tr><td>c</td><td>d</td></tr></tbody><tfoot><tr><td>end</td></tr></tfoot></table>",
            "| a | b |\n| --- | --- |\n| c | d |\n| end |",
        ),
        (
            "<table><tr><th>a</th><th>b</th></tr><tr></tr><tr><td>c</td><td>d</td></tr></table>",
            "| a | b |\n| --- | --- |\n| c | d |",
        ),
    ] {
        assert_eq!(markdown(html), expected, "{html}");
    }
}

#[test]
fn data_table_outputs_gfm_and_preserves_literal_underscores() {
    let actual = markdown(include_str!("fixtures/data-table.html"));
    assert_eq!(actual, include_str!("fixtures/data-table.md").trim_end());
    assert_eq!(
        rendered_html(&actual),
        "<table><thead><tr><th>Name</th><th>Age</th></tr></thead><tbody>\n<tr><td>Alice</td><td>30</td></tr>\n<tr><td>Bob_under</td><td>12</td></tr>\n</tbody></table>\n"
    );
}

#[test]
fn headerless_data_table_keeps_every_data_row_under_empty_headers() {
    let actual = markdown(include_str!("fixtures/headerless-data-table.html"));
    assert_eq!(
        actual,
        include_str!("fixtures/headerless-data-table.md").trim_end()
    );
    assert_eq!(
        rendered_html(&actual),
        "<table><thead><tr><th></th><th></th></tr></thead><tbody>\n<tr><td>苹果</td><td>5 元</td></tr>\n<tr><td>香蕉</td><td>3 元</td></tr>\n</tbody></table>\n"
    );
}

#[test]
fn headerless_tables_use_the_widest_row_without_discarding_cells() {
    let actual = markdown(
        "<table><tr></tr><tr><td align='right'>one</td></tr><tr><td>two</td><td align='center'>three</td></tr><tr><td>four</td></tr></table>",
    );
    assert_eq!(
        actual,
        "|  |  |\n| ---: | :---: |\n| one |\n| two | three |\n| four |"
    );
    let rendered = rendered_html(&actual);
    assert_eq!(rendered.matches("<td ").count(), 6);
    assert!(rendered.contains("<td style=\"text-align: center\">three</td>"));
}

#[test]
fn recognizes_headers_through_sections_whitespace_and_comments() {
    for header in [
        "<!-- before --><tr>\n<th>Name</th><!-- between --><th>Age</th>\n</tr>",
        "<thead>\n<tr><td>Name</td><td>Age</td></tr>\n</thead>",
        "<thead> \n<!-- empty --> </thead><tbody><tr><th>Name</th><th>Age</th></tr></tbody>",
    ] {
        let html =
            format!("<table>{header}<tbody><tr><td>Alice</td><td>30</td></tr></tbody></table>");
        assert_eq!(
            markdown(&html),
            "| Name | Age |\n| --- | --- |\n| Alice | 30 |",
            "{html}"
        );
    }
    assert_eq!(
        markdown(
            "<table><tr><th>Name</th><td>Age</td></tr><tr><td>Alice</td><td>30</td></tr></table>"
        ),
        "|  |  |\n| --- | --- |\n| Name | Age |\n| Alice | 30 |"
    );
}

#[test]
fn preserves_caption_alignment_empty_cells_and_short_rows() {
    let html = "<table><caption><strong>People</strong></caption><thead><tr><th align='LEFT'>Name</th><th align=' center '>Role</th><th align='right'>Age</th></tr></thead><tbody><tr><td>Alice</td><td></td><td>30</td></tr><tr><td>Bob</td></tr><tr><td></td><td></td><td></td></tr></tbody></table>";
    let actual = markdown(html);
    assert_eq!(
        actual,
        "**People**\n\n| Name | Role | Age |\n| :--- | :---: | ---: |\n| Alice |  | 30 |\n| Bob |\n|  |  |  |"
    );
    let rendered = rendered_html(&actual);
    assert!(
        rendered.contains("<th style=\"text-align: left\">Name</th>"),
        "{rendered}"
    );
    assert!(
        rendered.contains("<th style=\"text-align: center\">Role</th>"),
        "{rendered}"
    );
    assert!(
        rendered.contains("<th style=\"text-align: right\">Age</th>"),
        "{rendered}"
    );
    assert_eq!(rendered.matches("<td ").count(), 9);
}

#[test]
fn cells_preserve_inline_formatting_links_and_line_breaks() {
    let html = "<table><tr><th>Name</th><th>Details</th></tr><tr><td><a href='/a?x=1&amp;y=2'><strong>Alice</strong></a></td><td>first<br>second<div><em>third</em></div><p>fourth</p></td></tr></table>";
    let actual = markdown(html);
    assert_eq!(
        actual,
        "| Name | Details |\n| --- | --- |\n| [**Alice**](/a?x=1&amp;y=2) | first<br>second<br><br>*third*<br><br>fourth |"
    );
    assert!(
        rendered_html(&actual)
            .contains("<td>first<br>second<br><br><em>third</em><br><br>fourth</td>")
    );
}

#[test]
fn literal_pipes_survive_in_text_code_links_and_image_attributes() {
    for slashes in 0..=3 {
        let content = format!("a{}|b", "\\".repeat(slashes));
        let html = format!(
            "<table><tr><th>Text</th><th>Code</th><th>Link</th><th>Image</th></tr><tr><td>{content}</td><td><code>{content}</code></td><td><a href='/a|b' title='a|b'>{content}</a></td><td><img src='/i|j' alt='{content}' title='a|b'></td></tr></table>"
        );
        let actual = markdown(&html);
        let rendered = rendered_html(&actual);
        assert_eq!(rendered.matches("<th>").count(), 4, "{actual}");
        assert_eq!(rendered.matches("<td>").count(), 4, "{actual}");
        assert!(
            rendered.contains(&format!("<td>{content}</td>")),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("<code>{content}</code>")),
            "{rendered}"
        );
        assert!(
            rendered.contains("href=\"/a%7Cb\" title=\"a|b\""),
            "{rendered}"
        );
        assert!(
            rendered.contains(&format!("alt=\"{content}\"")),
            "{rendered}"
        );
    }
}

#[test]
fn unrepresentable_tables_expand_without_losing_or_duplicating_content() {
    for row in [
        "<tr><td colspan='2'>one</td><td>two</td></tr>",
        "<tr><td rowspan='2'>one</td><td>two</td></tr>",
        "<tr><td rowspan='0'>one</td><td>two</td></tr>",
        "<tr><td colspan='999999999999999999999'>one</td><td>two</td></tr>",
        "<tr><td>one</td><td>two</td><td>extra</td></tr>",
    ] {
        let actual = markdown(&format!(
            "<table><tr><th>A</th><th>B</th></tr>{row}</table>"
        ));
        let extra = if row.contains("extra") {
            "\n\nextra"
        } else {
            ""
        };
        assert_eq!(actual, format!("A\n\nB\n\none\n\ntwo{extra}"));
    }
    for content in [
        "<pre>  code\nnext\n</pre>",
        "<ul><li>item</li></ul>",
        "<h2>Title</h2>",
        "<blockquote>quote</blockquote>",
        "<table><tr><td>nested</td></tr></table>",
    ] {
        for cell in ["th", "td"] {
            let actual = markdown(&format!(
                "<table><tr><{cell}>A</{cell}><{cell}>B</{cell}></tr><tr><td>one</td><td>{content}</td></tr></table>"
            ));
            assert_eq!(
                actual,
                format!("A\n\nB\n\none\n\n{}", markdown(content)),
                "{cell}: {content}"
            );
        }
    }
}

#[test]
fn table_blocks_work_inside_lists_and_blockquotes() {
    let table = "<table><tr><th>A</th><th>B</th></tr><tr><td>one</td><td>two</td></tr></table>";
    for wrapper in [
        format!("<blockquote>{table}</blockquote>"),
        format!("<ul><li>{table}</li></ul>"),
    ] {
        let actual = markdown(&wrapper);
        let rendered = rendered_html(&actual);
        assert_eq!(rendered.matches("<table>").count(), 1, "{actual}");
        assert_eq!(rendered.matches("<td>").count(), 2, "{actual}");
    }
}

#[test]
fn table_blocks_preserve_surrounding_inline_styles() {
    for (tag, open, close) in [
        ("strong", "**", "**"),
        ("a href='/outer'", "[", "](/outer)"),
    ] {
        let end_tag = tag.split(' ').next().unwrap();
        let actual = markdown(&format!(
            "<{tag}>before<table><tr><td>cell</td></tr></table>after</{end_tag}>"
        ));
        assert_eq!(
            actual,
            format!(
                "{open}before{close}\n\n|  |\n| --- |\n| {open}cell{close} |\n\n{open}after{close}"
            )
        );
    }
}

#[test]
fn multiple_heading_rows_fall_back_to_blocks() {
    assert_eq!(
        markdown(
            "<table><thead><tr><th>A</th><th>B</th></tr><tr><th>one</th><th>two</th></tr></thead><tbody><tr><td>three</td><td>four</td></tr></tbody></table>"
        ),
        "A\n\nB\n\none\n\ntwo\n\nthree\n\nfour"
    );
}

#[test]
fn footer_only_tables_keep_the_footer_as_data() {
    assert_eq!(
        markdown("<table><tfoot><tr><th>Footer</th></tr></tfoot></table>"),
        "|  |\n| --- |\n| Footer |"
    );
}

#[test]
fn empty_headers_and_unit_spans_are_representable() {
    let actual = markdown(
        "<table><tr><th></th><th colspan='1' rowspan='1'></th></tr><tr><td>one</td><td>two</td></tr></table>",
    );
    assert_eq!(actual, "|  |  |\n| --- | --- |\n| one | two |");
    assert!(rendered_html(&actual).contains("<td>one</td><td>two</td>"));
    assert_eq!(markdown("<table></table>"), "");
    assert_eq!(markdown("<table><tr></tr></table>"), "");
}

#[test]
fn short_rows_do_not_expand_to_a_large_grid() {
    for cell in ["th", "td"] {
        let html = format!(
            "<table><tr>{}</tr>{}</table>",
            format!("<{cell}>Header</{cell}>").repeat(64),
            "<tr><td>cell</td></tr>".repeat(64)
        );
        let actual = markdown(&html);
        assert!(
            actual.len() < html.len(),
            "short rows must not be padded to the header width"
        );
        let rendered = rendered_html(&actual);
        assert_eq!(rendered.matches("<td>cell</td>").count(), 64);
        let data_rows = if cell == "th" { 64 } else { 65 };
        assert_eq!(rendered.matches("<td>").count(), data_rows * 64);
    }
}

#[test]
fn table_cells_respect_the_depth_limit() {
    let dom = Tree::parse(
        "<table><tr><th>Heading</th></tr><tr><td>visible<em>hidden</em></td></tr></table>",
    );
    let converter = Converter::new(Options {
        max_depth: 6,
        ..Options::default()
    });
    assert_eq!(
        converter.convert(&dom, dom.root),
        "| Heading |\n| --- |\n| visible |"
    );
    assert_eq!(
        convert(&dom, dom.root),
        "| Heading |\n| --- |\n| visible*hidden* |"
    );
}

#[test]
fn table_presentation_attributes_do_not_change_conversion() {
    for attributes in [
        "",
        "border='0' width='100%'",
        "border='1'",
        "style='border:0'",
    ] {
        let html = format!(
            "<table {attributes}><tr><td><a href='/'>Home</a></td><td><a href='/help'>Help</a></td></tr></table>"
        );
        assert_eq!(
            markdown(&html),
            "|  |  |\n| --- | --- |\n| [Home](/) | [Help](/help) |"
        );
    }
}

#[test]
fn nested_tables_obey_the_conversion_depth_limit() {
    let dom = Tree::parse(
        "<table><tr><td>visible<div><table><tr><td>too deep</td></tr></table></div></td></tr></table>",
    );
    for (max_depth, expected) in [(6, "|  |\n| --- |\n| visible |"), (7, "visible")] {
        let converter = Converter::new(Options {
            max_depth,
            ..Options::default()
        });
        assert_eq!(converter.convert(&dom, dom.root), expected);
    }
    assert_eq!(
        convert(&dom, dom.root),
        "visible\n\n|  |\n| --- |\n| too deep |"
    );
}
