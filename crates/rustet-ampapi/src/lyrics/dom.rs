//! A minimal XML tree for Apple's TTML.
//!
//! Apple's TTML is not always namespace-well-formed (prefixes such as `itunes:` may be
//! undeclared), so names are kept raw, split at the first `:`, and never resolved. A name
//! query without a prefix matches any prefix. Searches and text extraction follow the
//! semantics the Go engine relied on (beevik/etree): direct-child lookups, breadth-first
//! document-wide searches, and `text()` = character data before the first child element.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use std::collections::VecDeque;

use super::TtmlError;

#[derive(Debug, Default)]
pub(super) struct Element {
    prefix: String,
    name: String,
    attrs: Vec<Attr>,
    children: Vec<Node>,
}

#[derive(Debug)]
struct Attr {
    prefix: String,
    key: String,
    value: String,
}

#[derive(Debug)]
pub(super) enum Node {
    Element(Element),
    /// One run of character data. Runs split by markup (comments, CDATA) stay separate.
    Text(String),
}

/// Splits `prefix:name`; a query without a prefix matches any prefix.
fn split_name(qname: &str) -> (&str, &str) {
    qname.split_once(':').unwrap_or(("", qname))
}

fn name_matches(query: &str, prefix: &str, name: &str) -> bool {
    let (q_prefix, q_name) = split_name(query);
    (q_prefix.is_empty() || q_prefix == prefix) && q_name == name
}

impl Element {
    pub(super) fn local_name(&self) -> &str {
        &self.name
    }

    /// The value of attribute `key` (`"begin"`, `"itunes:key"`).
    pub(super) fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|a| name_matches(key, &a.prefix, &a.key))
            .map(|a| a.value.as_str())
    }

    pub(super) fn children(&self) -> &[Node] {
        &self.children
    }

    pub(super) fn child_elements(&self) -> impl Iterator<Item = &Element> {
        self.children.iter().filter_map(|n| match n {
            Node::Element(e) => Some(e),
            Node::Text(_) => None,
        })
    }

    pub(super) fn children_named<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a Element> {
        self.child_elements()
            .filter(move |e| name_matches(name, &e.prefix, &e.name))
    }

    pub(super) fn child(&self, name: &str) -> Option<&Element> {
        self.children_named(name).next()
    }

    /// The first direct child `text` element whose `for` attribute is `key`.
    pub(super) fn text_for(&self, key: &str) -> Option<&Element> {
        self.children_named("text")
            .find(|e| e.attr("for") == Some(key))
    }

    /// Character data before the first child element (comments are not kept, so they
    /// are skipped as etree skips them).
    pub(super) fn text(&self) -> String {
        self.children
            .iter()
            .map_while(|n| match n {
                Node::Text(t) => Some(t.as_str()),
                Node::Element(_) => None,
            })
            .collect()
    }

    /// Breadth-first over this element and all descendants, this element first.
    fn descendants_bfs(&self) -> impl Iterator<Item = &Element> {
        let mut queue = VecDeque::from([self]);
        std::iter::from_fn(move || {
            let e = queue.pop_front()?;
            queue.extend(e.child_elements());
            Some(e)
        })
    }

    /// Every element named `name` anywhere below this one, in etree's `//name` order:
    /// parents visited breadth-first, each parent's matching children in order.
    pub(super) fn find_all<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a Element> {
        self.descendants_bfs()
            .flat_map(move |e| e.children_named(name))
    }
}

/// A parsed document. The returned element is the document node: its children are the
/// top-level nodes.
pub(super) fn parse(xml: &str) -> Result<Element, TtmlError> {
    // encoding/xml normalises line endings in text and attribute values.
    let xml = xml.replace("\r\n", "\n").replace('\r', "\n");
    let mut reader = Reader::from_str(&xml);
    let mut stack = vec![Element::default()];
    // Whether the last node pushed is a text run that the next text may extend.
    let mut open_text = false;

    loop {
        let event = reader.read_event()?;
        let continues_text = matches!(event, Event::Text(_) | Event::GeneralRef(_));
        match event {
            Event::Start(start) => stack.push(element(&start)?),
            Event::Empty(start) => push_node(&mut stack, Node::Element(element(&start)?)),
            Event::End(_) => {
                let done = stack
                    .pop()
                    .filter(|_| !stack.is_empty())
                    .ok_or(TtmlError::Malformed)?;
                push_node(&mut stack, Node::Element(done));
            }
            Event::Text(t) => push_text(&mut stack, open_text, &t.decode()?),
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref()? {
                    Some(c) => c.to_string(),
                    None => {
                        let name = r.decode()?;
                        quick_xml::escape::resolve_predefined_entity(&name)
                            .ok_or_else(|| TtmlError::UnknownEntity(name.into_owned()))?
                            .to_owned()
                    }
                };
                push_text(&mut stack, open_text, &resolved);
            }
            Event::CData(c) => push_text(&mut stack, false, &c.decode()?),
            Event::Eof => break,
            Event::Comment(_) | Event::Decl(_) | Event::PI(_) | Event::DocType(_) => {}
        }
        open_text = continues_text;
    }

    match <[Element; 1]>::try_from(stack) {
        Ok([doc]) => Ok(doc),
        Err(_) => Err(TtmlError::Malformed), // unclosed elements at EOF
    }
}

fn element(start: &BytesStart<'_>) -> Result<Element, TtmlError> {
    let qname = std::str::from_utf8(start.name().into_inner()).map_err(|_| TtmlError::Malformed)?;
    let (prefix, name) = split_name(qname);
    let attrs = start
        .attributes()
        .map(|a| {
            let a = a?;
            let qkey = std::str::from_utf8(a.key.into_inner()).map_err(|_| TtmlError::Malformed)?;
            let (prefix, key) = split_name(qkey);
            Ok(Attr {
                prefix: prefix.to_owned(),
                key: key.to_owned(),
                value: a.unescape_value()?.into_owned(),
            })
        })
        .collect::<Result<_, TtmlError>>()?;
    Ok(Element {
        prefix: prefix.to_owned(),
        name: name.to_owned(),
        attrs,
        children: Vec::new(),
    })
}

fn push_node(stack: &mut [Element], node: Node) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    }
}

fn push_text(stack: &mut [Element], extend: bool, text: &str) {
    let Some(parent) = stack.last_mut() else {
        return;
    };
    if extend && let Some(Node::Text(prev)) = parent.children.last_mut() {
        prev.push_str(text);
        return;
    }
    parent.children.push(Node::Text(text.to_owned()));
}
