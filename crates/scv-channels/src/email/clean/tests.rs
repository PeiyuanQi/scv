//! Unit tests for `src/email/clean.rs`.

use super::*;

/// Every character `sanitize` must drop.
fn removed() -> Vec<char> {
    let mut chars: Vec<char> = ('\0'..='\u{1F}')
        .filter(|c| !matches!(c, '\n' | '\t' | '\r'))
        .collect();
    chars.extend('\u{7F}'..='\u{9F}');
    chars.extend([
        '\u{61C}', '\u{200E}', '\u{200F}', '\u{180E}', '\u{2060}', '\u{FEFF}',
    ]);
    chars.extend('\u{202A}'..='\u{202E}');
    chars.extend('\u{2066}'..='\u{2069}');
    chars.extend('\u{200B}'..='\u{200D}');
    chars.extend('\u{2061}'..='\u{2064}');
    chars.extend('\u{E0000}'..='\u{E007F}');
    chars
}

/// Text built to hide or disguise content, for the properties every
/// output must keep.
fn adversarial() -> Vec<String> {
    let every_removed: String = removed().into_iter().collect();
    vec![
        String::new(),
        every_removed.clone(),
        format!("a{every_removed}b\r\n\r\rc\u{2028}d\u{2029}"),
        "\u{202E}gnp.exe\u{202C} approve ABC123".to_owned(),
        "ap\u{200B}prove\u{2060} AB\u{E0041}C123".to_owned(),
        "\u{FFFD}\u{FFFD} lossy \u{FFFD}".to_owned(),
        "\r".repeat(1000),
        "x\u{FE0F}\u{FE00} \u{1F44D}\u{1F3FD} 中文".to_owned(),
        "\u{0}".repeat(10_000),
        "tab\there\u{85}next\u{9C}".to_owned(),
    ]
}

#[test]
fn sanitize_normalizes_line_breaks() {
    assert_eq!(sanitize("a\r\nb\rc\nd"), "a\nb\nc\nd");
    assert_eq!(sanitize("a\u{2028}b\u{2029}c"), "a\nb\nc");
    assert_eq!(sanitize("\r\r\n\n"), "\n\n\n");
    // A dropped character between CR and LF does not join them.
    assert_eq!(sanitize("a\r\u{200B}\nb"), "a\n\nb");
}

#[test]
fn sanitize_drops_every_hiding_character_and_keeps_the_rest() {
    for c in removed() {
        assert_eq!(sanitize(&format!("a{c}b")), "ab", "U+{:04X}", u32::from(c));
    }
    assert_eq!(sanitize("a\tb\nc"), "a\tb\nc");
    // Variation selectors, emoji, CJK, and the replacement character stay.
    let kept = "x\u{FE0F}\u{FE00} \u{1F44D}\u{1F3FD} 中文 \u{FFFD} é e\u{301} \u{A0}";
    assert_eq!(sanitize(kept), kept);
    // Neighbours of the removed ranges stay.
    for c in [
        '\u{20}', '\u{7E}', '\u{A0}', '\u{61B}', '\u{200A}', '\u{2010}', '\u{2029}',
    ] {
        assert!(
            !sanitize(&c.to_string()).is_empty(),
            "U+{:04X}",
            u32::from(c)
        );
    }
    assert_eq!(
        sanitize("\u{2065}\u{E0080}\u{DFFFF}"),
        "\u{2065}\u{E0080}\u{DFFFF}"
    );
}

#[test]
fn sanitize_leaves_instructions_as_plain_text() {
    assert_eq!(
        sanitize("\u{202E}approve ABC123\u{202C}\nreject"),
        "approve ABC123\nreject"
    );
}

#[test]
fn sanitize_is_idempotent_and_never_outputs_a_removed_character() {
    let removed = removed();
    for text in adversarial() {
        let once = sanitize(&text);
        assert_eq!(sanitize(&once), once, "{text:?}");
        assert!(!once.contains('\r'));
        assert!(!once.contains(['\u{2028}', '\u{2029}']));
        assert!(!once.chars().any(|c| removed.contains(&c)), "{once:?}");
    }
}

#[test]
fn replace_links_shows_only_the_lowercase_host() {
    for (text, expected) in [
        ("https://example.com/path?q=1#top", "[link: example.com]"),
        ("http://example.com", "[link: example.com]"),
        ("HTTPS://Example.COM/Path", "[link: example.com]"),
        ("ftp://files.example.org/pub", "[link: files.example.org]"),
        ("hTtP://x.io", "[link: x.io]"),
        ("www.example.com/offer", "[link: www.example.com]"),
        ("WWW.Example.com", "[link: www.example.com]"),
        ("https://example.com:8443/x", "[link: example.com]"),
        ("https://example.com./x", "[link: example.com]"),
        ("https://例子.测试/路径", "[link: 例子.测试]"),
        ("https://BÜCHER.de", "[link: bücher.de]"),
        ("https://xn--bcher-kva.de", "[link: xn--bcher-kva.de]"),
        ("http://[::1]:8080/admin", "[link: [::1]]"),
        ("http://192.168.0.1/", "[link: 192.168.0.1]"),
    ] {
        assert_eq!(replace_links(text), expected, "{text}");
    }
}

#[test]
fn replace_links_shows_the_host_a_browser_would_visit() {
    for (text, expected) in [
        ("http://user@evil.com", "[link: evil.com]"),
        ("https://paypal.com@evil.com/login", "[link: evil.com]"),
        ("http://a:b@evil.com:8080/x", "[link: evil.com]"),
        ("http://paypal.com:pw@x@evil.com/", "[link: evil.com]"),
        ("http://evil.com\\@good.com", "[link: evil.com]"),
        ("http://evil.com?@good.com", "[link: evil.com]"),
        ("http://evil.com#@good.com", "[link: evil.com]"),
    ] {
        assert_eq!(replace_links(text), expected, "{text}");
    }
}

#[test]
fn replace_links_marks_a_link_without_a_valid_host() {
    for text in [
        "http://",
        "https:///path",
        "http://%65vil.com",
        "https://:80",
        "http://host:port/",
        "http://[zz]/",
        "http://[]/",
        "http://-.../",
        "http://user@/",
        "http://evil\u{200B}.com",
    ] {
        assert_eq!(replace_links(text), "[link]", "{text}");
    }
}

#[test]
fn replace_links_leaves_trailing_punctuation_outside() {
    for (text, expected) in [
        ("see https://x.com/a.", "see [link: x.com]."),
        ("see https://x.com/a, then", "see [link: x.com], then"),
        ("(https://x.com/a)", "([link: x.com])"),
        ("[https://x.com/a]", "[[link: x.com]]"),
        ("{https://x.com}", "{[link: x.com]}"),
        ("https://x.com/wiki/A_(b))", "[link: x.com])"),
        ("go: https://x.com/?!;:", "go: [link: x.com]?!;:"),
        ("'https://x.com'", "'[link: x.com]'"),
        ("请看https://x.com/a。谢谢", "请看[link: x.com]。谢谢"),
        (
            "访问https://x.com，然后（https://y.cn）",
            "访问[link: x.com]，然后（[link: y.cn]）",
        ),
        ("「https://x.com」", "「[link: x.com]」"),
        ("【https://x.com】！", "【[link: x.com]】！"),
        ("“https://x.com/”", "“[link: x.com]”"),
        ("http://x.com)))))", "[link: x.com])))))"),
    ] {
        assert_eq!(replace_links(text), expected, "{text}");
    }
}

#[test]
fn replace_links_ends_a_url_at_whitespace_and_delimiters() {
    assert_eq!(
        replace_links("a https://x.com/p b\thttp://y.com\nwww.z.com"),
        "a [link: x.com] b\t[link: y.com]\n[link: www.z.com]"
    );
    assert_eq!(
        replace_links("href=\"https://x.com/a\">text"),
        "href=\"[link: x.com]\">text"
    );
    assert_eq!(replace_links("https://x.com<b>"), "[link: x.com]<b>");
    // Angle brackets around a URL go with it.
    assert_eq!(
        replace_links("Docs <https://x.com/a> here"),
        "Docs [link: x.com] here"
    );
    assert_eq!(replace_links("<https://x.com/a"), "<[link: x.com]");
    assert_eq!(
        replace_links("<https://a.com><https://b.com>"),
        "[link: a.com][link: b.com]"
    );
}

#[test]
fn replace_links_leaves_other_text_alone() {
    for text in [
        "mailto:bob@example.com",
        "Write to bob@www.example.com today",
        "awww.example.com",
        "www.",
        "www. example",
        "the www is big",
        "http:/x.com",
        "approve ABC123",
        "中文 без ссылок",
        "",
    ] {
        assert_eq!(replace_links(text), text, "{text}");
    }
}

#[test]
fn replace_links_stays_linear_on_hostile_input() {
    let cases = [
        "http://".repeat(150_000),
        format!("http://x.com/{}", ")".repeat(1 << 20)),
        format!("http://x.com/{}", "(".repeat(1 << 20)),
        "www.a".repeat(200_000),
        "wwww.".repeat(200_000),
        format!("https://{}", "a@".repeat(500_000)),
        format!("https://{}", "a.".repeat(500_000)),
    ];
    for text in cases {
        let start = std::time::Instant::now();
        let out = replace_links(&text);
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert!(out.len() <= text.len() + 64 || out.contains("[link"));
    }
}

#[test]
fn html_to_text_keeps_what_a_reader_sees() {
    let html = "<html><head><title>Title</title><style>p { color: red }</style>\
        <script>alert(1)</script></head><body>\
        <p>Hello <b>world</b> &amp; <a href=\"https://evil.com/x\">click here</a></p>\
        <script>var secret = 1;</script><style>.x{}</style>\
        <img src=\"logo.png\" alt=\"Logo\"><img src=\"pixel.gif\">\
        <p>Second <i>para</i> <s>struck</s> <code>code</code></p></body></html>";
    let text = html_to_text(html);
    // A link's real target follows its text, for `replace_links` to show.
    assert!(
        text.contains("Hello world & click here https://evil.com/x"),
        "{text:?}"
    );
    assert!(text.contains("Logo"));
    assert!(text.contains("Second para struck code"));
    for absent in [
        "Title", "color", "alert", "secret", "logo.png", "pixel", "[1]", "**", "*para*", "`",
    ] {
        assert!(!text.contains(absent), "{absent} in {text:?}");
    }
}

#[test]
fn html_to_text_marks_quotes_and_lists_and_reads_tables_by_cell() {
    let text = html_to_text(
        "<div>Hi</div><blockquote>quoted<br>more</blockquote>\
         <ul><li>one</li><li>two</li></ul><ol><li>first</li></ol><h1>Heading</h1>\
         <table><tr><td>Item</td><td>Price</td></tr><tr><td>Widget</td><td>$5</td></tr></table>",
    );
    let lines: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(
        lines,
        [
            "Hi", "> quoted", "> more", "- one", "- two", "1. first", "Heading", "Item", "Price",
            "Widget", "$5"
        ]
    );
}

#[test]
fn html_to_text_does_not_wrap_long_paragraphs() {
    let words = "word ".repeat(20_000);
    let text = html_to_text(&format!("<p>{words}</p>"));
    assert_eq!(text.trim(), words.trim());
}

#[test]
fn html_to_text_survives_malformed_html() {
    for html in [
        "",
        "plain text, no tags",
        "<div><p>unclosed <b>bold <i>both",
        "<<<>>> < > </ > <3 a<b",
        "</x></y></z>",
        "<!-- unterminated comment",
        "<script>never closed",
        "<table><td>cell<tr><th>",
        "<a href=\"javascript:alert(1)\">x</a",
        "&#0; &#xD800; &#99999999; &bogus; &",
        "<svg><title>t</title><path d=\"M0\"/></svg>",
        "\u{0}\u{FFFD}<p>\u{202E}text</p>",
    ] {
        let _ = html_to_text(html);
    }
    assert!(html_to_text("<div><p>unclosed <b>bold").contains("unclosed bold"));
}

#[test]
fn html_to_text_reads_deep_nesting_without_the_renderer() {
    for (open, close) in [
        ("<div>", "</div>"),
        ("<b>", "</b>"),
        ("<blockquote>", "</blockquote>"),
        ("<table><tr><td>", "</td></tr></table>"),
        ("<div/>", ""),
    ] {
        let html = format!("{}deep text{}", open.repeat(50_000), close.repeat(50_000));
        assert!(html.find("deep text").unwrap() < MAX_HTML_BYTES);
        let start = std::time::Instant::now();
        let text = html_to_text(&html);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "{open}"
        );
        assert!(text.contains("deep text"), "{open}");
    }
    // Nesting within the limit still goes through the renderer, which
    // marks the quotes.
    let html = format!(
        "{}q{}",
        "<blockquote>".repeat(10),
        "</blockquote>".repeat(10)
    );
    assert_eq!(
        html_to_text(&html).trim(),
        format!("{}q", "> ".repeat(10)).trim()
    );
}

#[test]
fn html_to_text_reads_at_most_a_mebibyte() {
    let html = format!("<p>{}</p>", "é".repeat(1 << 20));
    let text = html_to_text(&html);
    assert!(text.len() <= MAX_HTML_BYTES);
    assert!(text.starts_with("éé"));
}

#[test]
fn nesting_depth_counts_what_can_nest() {
    assert_eq!(nesting_depth(""), 0);
    assert_eq!(nesting_depth("<div><div></div></div><div>"), 2);
    assert_eq!(nesting_depth("<p><p><li><li><br><img><td><tr>"), 0);
    assert_eq!(nesting_depth("<!-- <div><div> --><span>"), 1);
    assert_eq!(nesting_depth("</div></div><b>"), 1);
    assert_eq!(nesting_depth("< div><1><DIV/><Div>"), 2);
}

#[test]
fn strip_tags_keeps_text_and_line_structure() {
    let text = strip_tags(
        "<!DOCTYPE html><html><head><title>T</title></head><body>\
         <style>p{}</style><SCRIPT>x()</script ><!-- note -->\
         <p>One  &amp;\n two</p><div>Three&nbsp;&lt;4&gt; &#x4E2D;&#25991;</div>\
         <table><tr><td>a</td><td>b</td></tr></table>1 < 2 <3</body></html>",
    );
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        lines,
        ["One & two", "Three\u{A0}<4> 中文", "a  b", "1 < 2 <3"]
    );
    assert_eq!(strip_tags("<head>x</header>y</head>z"), "z");
    assert_eq!(strip_tags("<script>no end"), "");
    assert_eq!(strip_tags("a<!-- no end"), "a");
    assert_eq!(
        strip_tags("&#0;&#xFFFFFF;&#;&x;&am"),
        "\u{FFFD}\u{FFFD}&#;&x;&am"
    );
}

#[test]
fn clean_body_converts_html_first() {
    let cleaned = clean_body(
        "<p>Your code is <b>ready</b>: <a href=\"https://evil.com/x\">the portal</a>, \
         or https://Bank.com/login</p><style>.x{}</style>",
        true,
        4096,
    );
    assert_eq!(
        cleaned.text,
        "Your code is ready: the portal [link: evil.com], or [link: bank.com]"
    );
    assert!(!cleaned.truncated);
}

#[test]
fn clean_body_cuts_history_from_webmail_html() {
    let gmail = "<div dir=\"ltr\">Thanks!<br></div><br><div class=\"gmail_quote\">\
        <div dir=\"ltr\" class=\"gmail_attr\">On Mon, Jan 5, 2026 at 10:00 AM Ann &lt;\
        <a href=\"mailto:ann@example.com\">ann@example.com</a>&gt; wrote:<br></div>\
        <blockquote class=\"gmail_quote\">old <a href=\"https://x.com\">link</a></blockquote></div>";
    assert_eq!(clean_body(gmail, true, 4096).text, "Thanks!");
    let qq = "<div>好的</div><div><br></div><div style=\"font-size: 12px;\">\
        ------------------&nbsp;原始邮件&nbsp;------------------</div>\
        <div style=\"background:#efefef;\"><div><b>发件人:</b>&nbsp;\"张三\"&lt;zhang@example.com&gt;;</div>\
        <div><b>发送时间:</b>&nbsp;2026年1月5日(星期一) 上午10:00</div></div><div>旧内容</div>";
    assert_eq!(clean_body(qq, true, 4096).text, "好的");
    let outlook = "<div><p class=\"MsoNormal\">Approved.</p></div><div style=\"border-top:solid\">\
        <p class=\"MsoNormal\"><b>From:</b> Ann &lt;ann@example.com&gt;<br><b>Sent:</b> Monday, \
        January 5, 2026 10:00 AM<br><b>To:</b> Bob<br><b>Subject:</b> Plan</p></div><p>old</p>";
    assert_eq!(clean_body(outlook, true, 4096).text, "Approved.");
}

#[test]
fn clean_body_drops_quoted_lines() {
    let cleaned = clean_body(
        "Top reply\n> quoted\n  > indented quote\n>> nested\nafter",
        false,
        4096,
    );
    assert_eq!(cleaned.text, "Top reply\nafter");
}

#[test]
fn clean_body_cuts_at_english_attributions() {
    let single = "Sounds good.\n\nOn Mon, Jan 5, 2026 at 10:00 AM Ann <ann@example.com> wrote:\n\
                  > earlier\nnot quoted but history";
    assert_eq!(clean_body(single, false, 4096).text, "Sounds good.");
    let wrapped = "Sounds good.\nOn Mon, Jan 5, 2026 at 10:00 AM Ann Example <\nann@example.com> wrote:\n\
                   history";
    assert_eq!(clean_body(wrapped, false, 4096).text, "Sounds good.");
    // Prose that merely starts with "On" is kept.
    let prose = "On Monday we meet.\nHere is what Bob wrote:\nthe plan";
    assert_eq!(clean_body(prose, false, 4096).text, prose);
    let prose = "On the forum, Bob wrote:\n> a quote\nWhat do you think?";
    assert_eq!(
        clean_body(prose, false, 4096).text,
        "On the forum, Bob wrote:\nWhat do you think?"
    );
}

#[test]
fn clean_body_cuts_at_marker_lines() {
    for marker in [
        "-----Original Message-----",
        "----- Original Message -----",
        "------------------ 原始邮件 ------------------",
        "------------------\u{A0}Original\u{A0}------------------",
        "---- 回复的原邮件 ----",
        "-----邮件原件-----",
    ] {
        let body = format!("Reply text\n{marker}\nFrom: someone\nold text");
        assert_eq!(
            clean_body(&body, false, 4096).text,
            "Reply text",
            "{marker}"
        );
    }
    // A forwarded message is the content, not history.
    let forward = "FYI\n---------- Forwarded message ---------\nFrom: a\nThe news";
    assert_eq!(clean_body(forward, false, 4096).text, forward);
}

#[test]
fn clean_body_cuts_at_header_blocks() {
    let outlook = "Reply\n\n________________________________\nFrom: Ann <ann@example.com>\n\
                   Sent: Monday, January 5, 2026 10:00 AM\nTo: Bob\nSubject: Plan\n\nold";
    assert_eq!(clean_body(outlook, false, 4096).text, "Reply");
    let dated = "Reply\nFrom: Ann\nDate: 2026-01-05\nSubject: Plan\nold";
    assert_eq!(clean_body(dated, false, 4096).text, "Reply");
    let chinese = "好的\n\n发件人： 张三 <zhang@example.com>\n发送时间： 2026年1月5日 10:00\n收件人： 李四\n旧内容";
    assert_eq!(clean_body(chinese, false, 4096).text, "好的");
    let short = "好的\n发件人: 张三\n日期: 2026-01-05\n旧内容";
    assert_eq!(clean_body(short, false, 4096).text, "好的");
    // A lone `From:` line, or one without the rest of the block nearby, is text.
    let lone = "From: the whole team\nThanks for everything\nSent: with love";
    assert_eq!(clean_body(lone, false, 4096).text, lone);
    let far = "From: a\n1\n2\n3\n4\nSent: x\nTo: y";
    assert_eq!(clean_body(far, false, 4096).text, far);
}

#[test]
fn clean_body_cuts_at_chinese_attributions() {
    let gmail = "收到\n\n在 2026年1月5日周一 10:00，张三 <zhang@example.com> 写道：\n> 旧内容";
    assert_eq!(clean_body(gmail, false, 4096).text, "收到");
    let ascii_colon = "收到\n在 2026-01-05 张三 写道:\n旧内容";
    assert_eq!(clean_body(ascii_colon, false, 4096).text, "收到");
    let wrapped = "收到\n在 2026年1月5日周一 10:00，张三 <\nzhang@example.com> 写道：\n旧内容";
    assert_eq!(clean_body(wrapped, false, 4096).text, "收到");
    let prose = "在他的书中，作者写道：\n知识就是力量";
    assert_eq!(clean_body(prose, false, 4096).text, prose);
}

#[test]
fn clean_body_cuts_the_signature_after_the_first_line() {
    assert_eq!(
        clean_body("Hi\nBody\n-- \nAnn\n555-0100", false, 4096).text,
        "Hi\nBody"
    );
    assert_eq!(clean_body("Hi\n--\nAnn", false, 4096).text, "Hi");
    // Only an exact delimiter.
    assert_eq!(
        clean_body("Hi\n---\nmore", false, 4096).text,
        "Hi\n---\nmore"
    );
    assert_eq!(
        clean_body("Hi\n-- Ann\nmore", false, 4096).text,
        "Hi\n-- Ann\nmore"
    );
    // A body that starts with the delimiter is not emptied.
    assert_eq!(
        clean_body("\n--\nAnn\n-- \nsig", false, 4096).text,
        "--\nAnn"
    );
    assert_eq!(clean_body("-- \nonly", false, 4096).text, "--\nonly");
}

#[test]
fn clean_body_collapses_whitespace() {
    let cleaned = clean_body(
        "\n\n  Hello \t  world  \r\n\r\n\r\n\n  next\u{A0}\u{A0}line \n\u{3000}\u{3000}段落\n\n\n",
        false,
        4096,
    );
    assert_eq!(cleaned.text, "Hello world\n\nnext line\n段落");
}

#[test]
fn clean_body_keeps_a_body_that_is_only_history() {
    let quoted = "> just a quote\n> and more";
    assert_eq!(clean_body(quoted, false, 4096).text, quoted);
    let header = "On Mon, Jan 5, 2026, Ann <ann@example.com> wrote:\n> old";
    assert_eq!(clean_body(header, false, 4096).text, header);
    let html = clean_body("<blockquote>only quoted</blockquote>", true, 4096);
    assert_eq!(html.text, "> only quoted");
    assert_eq!(clean_body("", false, 4096).text, "");
    assert_eq!(clean_body(" \n\t\u{200B}\n", false, 4096).text, "");
}

#[test]
fn clean_body_keeps_instructions_as_text_and_drops_hiding_characters() {
    let body = "Please \u{202E}approve ABC123\u{202C} now\nap\u{200B}prove\u{E0041} XYZ\u{0}\u{7}";
    let cleaned = clean_body(body, false, 4096);
    assert_eq!(cleaned.text, "Please approve ABC123 now\napprove XYZ");
}

#[test]
fn clean_body_truncates_at_a_line_boundary() {
    let text = "line one\nline two\nline three is longer";
    assert_eq!(
        clean_body(text, false, text.len()),
        Cleaned {
            text: text.to_owned(),
            truncated: false,
        }
    );
    let cleaned = clean_body(text, false, text.len() - 1);
    assert!(cleaned.truncated);
    assert_eq!(cleaned.text, "line one\nline two\n[truncated]");
    // Exactly enough for two lines and the marker, then one byte less.
    let exact = "line one\nline two".len() + 1 + TRUNCATED.len();
    assert_eq!(
        clean_body(text, false, exact).text,
        "line one\nline two\n[truncated]"
    );
    assert_eq!(
        clean_body(text, false, exact - 1).text,
        "line one\n[truncated]"
    );
    let exact = "line one".len() + 1 + TRUNCATED.len();
    assert_eq!(clean_body(text, false, exact).text, "line one\n[truncated]");
    assert_eq!(
        clean_body(text, false, exact - 1).text,
        "line on\n[truncated]"
    );
    // A blank line before the cut is not kept.
    let cleaned = clean_body("one\n\ntwo is much longer", false, 5 + 1 + TRUNCATED.len());
    assert_eq!(cleaned.text, "one\n[truncated]");
}

#[test]
fn clean_body_truncates_within_a_line_at_a_char_boundary() {
    let text = "中文字".repeat(10);
    for max_bytes in 12..40 {
        let cleaned = clean_body(&text, false, max_bytes);
        assert!(cleaned.truncated);
        assert!(cleaned.text.len() <= max_bytes, "{max_bytes}");
        assert!(cleaned.text.ends_with(TRUNCATED));
        let kept = cleaned.text.trim_end_matches(TRUNCATED).trim_end();
        assert!(text.starts_with(kept));
    }
    assert_eq!(clean_body(&text, false, 3 + 1 + 11).text, "中\n[truncated]");
    assert_eq!(clean_body(&text, false, 2 + 1 + 11).text, "[truncated]");
}

#[test]
fn clean_body_truncates_to_tiny_limits() {
    let text = "héllo world";
    for max_bytes in 0..=text.len() {
        let cleaned = clean_body(text, false, max_bytes);
        assert!(cleaned.text.len() <= max_bytes, "{max_bytes}");
        assert_eq!(cleaned.truncated, max_bytes < text.len());
    }
    assert_eq!(clean_body(text, false, 0).text, "");
    assert_eq!(clean_body(text, false, 2).text, "h");
    assert_eq!(clean_body(text, false, 11).text, TRUNCATED);
}

#[test]
fn clean_body_handles_a_mebibyte_line() {
    let line = "a".repeat(1 << 20);
    let start = std::time::Instant::now();
    let cleaned = clean_body(&line, false, 8192);
    assert!(start.elapsed() < std::time::Duration::from_secs(10));
    assert!(cleaned.truncated);
    assert_eq!(cleaned.text.len(), 8192);
    let html = format!("<p>{}</p>", "word ".repeat(200_000));
    let cleaned = clean_body(&html, true, 8192);
    assert!(cleaned.truncated && cleaned.text.len() <= 8192);
}

#[test]
fn clean_body_output_is_bounded_and_clean_for_hostile_input() {
    let removed = removed();
    let mut inputs = adversarial();
    inputs.push("> a\n".repeat(1000));
    inputs.push("On 1 wrote:\n".repeat(1000));
    inputs.push("-- \n".repeat(1000));
    inputs.push(format!("x{}", "\n".repeat(100_000)));
    inputs.push("<https://a.b> ".repeat(1000));
    for text in inputs {
        for html in [false, true] {
            for max_bytes in [0, 5, 11, 12, 13, 64, 4096] {
                let cleaned = clean_body(&text, html, max_bytes);
                assert!(cleaned.text.len() <= max_bytes);
                assert!(
                    !cleaned
                        .text
                        .chars()
                        .any(|c| removed.contains(&c) || c == '\r')
                );
                assert!(!cleaned.text.contains("\n\n\n"));
                assert_eq!(cleaned.text.trim(), cleaned.text);
            }
        }
    }
}

#[test]
fn an_html_link_shows_where_it_really_goes() {
    let text = clean_body(
        "<p>Pay at <a href=\"https://evil.example/pay?x=1\">https://bank.example</a> today</p>\
         <p><a href=\"https://shop.example/\">https://shop.example/</a> and \
         <a href=\"mailto:a@b.example\">write</a> or <a href=\"javascript:alert(1)\">here</a></p>",
        true,
        4096,
    )
    .text;
    assert!(
        text.contains("[link: bank.example] [link: evil.example]"),
        "{text}"
    );
    // The same target written out as the text shows once.
    assert!(text.contains("[link: shop.example] and"), "{text}");
    assert!(
        !text.contains("javascript") && !text.contains("mailto"),
        "{text}"
    );
}
