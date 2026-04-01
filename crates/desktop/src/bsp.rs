use std::collections::HashMap;

#[derive(Clone, Debug)]
pub enum BspNode {
    Split {
        direction: Direction,
        children: Vec<BspChild>,
    },
    Leaf {
        tag: String,
        command: Option<String>,
    },
}

#[derive(Clone, Debug)]
pub struct BspChild {
    pub node: BspNode,
    pub weight: f32,
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Direction {
    Horizontal,
    Vertical,
    Tabs,
}

#[derive(Clone, Debug)]
pub struct Pane {
    pub id: String,
    pub tag: String,
    pub command: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PaneRect {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

pub struct BspLayout {
    pub name: String,
    pub dsl: String,
    pub root: BspNode,
    pub weight: f32,
    pub active_tabs: HashMap<String, usize>,
}

pub static PRESETS: &[(&str, &str)] = &[
    ("Side by side", "line(left, right)"),
    ("Tabs", "tabs(a, b, c)"),
    ("2-1 thirds", "line(main 2, side)"),
    ("Grid", "col(line(a, b), line(c, d))"),
    ("Dev", "line(editor 2, col(shell, logs))"),
    ("Dev + tabs", "line(editor 2, tabs(shell, logs, build))"),
    ("Split + tabs", "line(tabs(a, b) 2, tabs(c, d))"),
];

pub fn parse_dsl(input: &str) -> Result<(BspNode, f32), String> {
    let mut parser = Parser::new(input);
    let (node, weight, _, _) = parser.parse_entry()?;
    Ok((node, weight))
}

pub fn serialize_dsl(node: &BspNode) -> String {
    serialize_node(node)
}

pub fn enumerate_panes(node: &BspNode) -> Vec<Pane> {
    let mut panes = Vec::new();
    collect_panes(node, "", &mut panes);
    panes
}

pub fn layout_rects(node: &BspNode, x: f32, y: f32, w: f32, h: f32, active_tabs: &HashMap<String, usize>) -> Vec<PaneRect> {
    let mut rects = Vec::new();
    compute_rects(node, x, y, w, h, "", active_tabs, &mut rects);
    rects
}

pub fn assign_sessions<K: Clone + Eq + std::hash::Hash>(
    panes: &[Pane],
    live: &[K],
    focused: Option<&K>,
    lru: &[K],
) -> HashMap<String, Option<K>> {
    let mut used = std::collections::HashSet::<K>::new();
    let mut assignments = HashMap::new();
    let mut candidates: Vec<&K> = Vec::new();
    if let Some(f) = focused {
        candidates.push(f);
    }
    for s in lru {
        candidates.push(s);
    }
    for s in live {
        candidates.push(s);
    }

    for pane in panes {
        if pane.command.is_some() {
            assignments.insert(pane.id.clone(), None);
            continue;
        }
        let mut assigned = false;
        for c in &candidates {
            if !used.contains(*c) {
                used.insert((*c).clone());
                assignments.insert(pane.id.clone(), Some((*c).clone()));
                assigned = true;
                break;
            }
        }
        if !assigned {
            assignments.insert(pane.id.clone(), None);
        }
    }
    assignments
}

pub fn adjust_weights(children: &mut [BspChild], idx_a: usize, idx_b: usize, fraction: f32) {
    let total = children[idx_a].weight + children[idx_b].weight;
    let delta = fraction * total;
    children[idx_a].weight = (children[idx_a].weight + delta).max(0.1);
    children[idx_b].weight = (children[idx_b].weight - delta).max(0.1);
}

fn collect_panes(node: &BspNode, prefix: &str, panes: &mut Vec<Pane>) {
    match node {
        BspNode::Leaf { tag, command } => {
            let id = if prefix.is_empty() { "0".to_string() } else { prefix.to_string() };
            panes.push(Pane { id, tag: tag.clone(), command: command.clone() });
        }
        BspNode::Split { children, .. } => {
            for (i, child) in children.iter().enumerate() {
                let child_prefix = if prefix.is_empty() {
                    i.to_string()
                } else {
                    format!("{prefix}.{i}")
                };
                collect_panes(&child.node, &child_prefix, panes);
            }
        }
    }
}

fn compute_rects(
    node: &BspNode,
    x: f32, y: f32, w: f32, h: f32,
    prefix: &str,
    active_tabs: &HashMap<String, usize>,
    rects: &mut Vec<PaneRect>,
) {
    match node {
        BspNode::Leaf { .. } => {
            let id = if prefix.is_empty() { "0".to_string() } else { prefix.to_string() };
            rects.push(PaneRect { id, x, y, w, h });
        }
        BspNode::Split { direction, children } => {
            if *direction == Direction::Tabs {
                let active = active_tabs.get(prefix).copied().unwrap_or(0).min(children.len().saturating_sub(1));
                let child_prefix = if prefix.is_empty() {
                    active.to_string()
                } else {
                    format!("{prefix}.{active}")
                };
                let tab_bar_h = 24.0f32;
                compute_rects(&children[active].node, x, y + tab_bar_h, w, h - tab_bar_h, &child_prefix, active_tabs, rects);
                return;
            }
            let total_weight: f32 = children.iter().map(|c| c.weight).sum();
            let horizontal = *direction == Direction::Horizontal;
            let mut offset = 0.0f32;
            let total_size = if horizontal { w } else { h };
            for (i, child) in children.iter().enumerate() {
                let frac = child.weight / total_weight;
                let size = total_size * frac;
                let child_prefix = if prefix.is_empty() {
                    i.to_string()
                } else {
                    format!("{prefix}.{i}")
                };
                if horizontal {
                    compute_rects(&child.node, x + offset, y, size, h, &child_prefix, active_tabs, rects);
                } else {
                    compute_rects(&child.node, x, y + offset, w, size, &child_prefix, active_tabs, rects);
                }
                offset += size;
            }
        }
    }
}

fn serialize_node(node: &BspNode) -> String {
    match node {
        BspNode::Leaf { tag, command } => {
            let mut s = quote_if_needed(tag);
            if let Some(cmd) = command {
                s.push_str(&format!("={}", quote_if_needed(cmd)));
            }
            s
        }
        BspNode::Split { direction, children } => {
            let keyword = match direction {
                Direction::Horizontal => "line",
                Direction::Vertical => "col",
                Direction::Tabs => "tabs",
            };
            let parts: Vec<String> = children.iter().map(|c| {
                let mut s = String::new();
                if let Some(ref label) = c.label {
                    s.push_str(&quote_if_needed(label));
                    s.push_str(": ");
                }
                s.push_str(&serialize_node(&c.node));
                if (c.weight - 1.0).abs() > 0.001 {
                    s.push_str(&format!(" {}", c.weight));
                }
                s
            }).collect();
            format!("{keyword}({})", parts.join(", "))
        }
    }
}

fn quote_if_needed(s: &str) -> String {
    if s.contains(|c: char| c.is_whitespace() || "(),@'\"\\:=".contains(c)) {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.input.len() && self.input.as_bytes()[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.pos).copied()
    }

    fn expect(&mut self, ch: u8) -> Result<(), String> {
        self.skip_ws();
        if self.peek() == Some(ch) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}' at pos {}", ch as char, self.pos))
        }
    }

    fn parse_identifier(&mut self) -> Result<String, String> {
        self.skip_ws();
        if self.peek() == Some(b'"') || self.peek() == Some(b'\'') {
            return self.parse_quoted();
        }
        let start = self.pos;
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c.is_ascii_whitespace() || b"(),@'\"\\:=".contains(&c) {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            return Err(format!("expected identifier at pos {}", self.pos));
        }
        Ok(self.input[start..self.pos].to_string())
    }

    fn parse_quoted(&mut self) -> Result<String, String> {
        let quote = self.input.as_bytes()[self.pos];
        self.pos += 1;
        let mut s = String::new();
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c == b'\\' && self.pos + 1 < self.input.len() {
                self.pos += 1;
                s.push(self.input.as_bytes()[self.pos] as char);
            } else if c == quote {
                self.pos += 1;
                return Ok(s);
            } else {
                s.push(c as char);
            }
            self.pos += 1;
        }
        Err("unterminated string".into())
    }

    fn parse_number(&mut self) -> Option<f32> {
        self.skip_ws();
        let start = self.pos;
        while self.pos < self.input.len() {
            let c = self.input.as_bytes()[self.pos];
            if c.is_ascii_digit() || c == b'.' {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos > start {
            self.input[start..self.pos].parse().ok()
        } else {
            None
        }
    }

    fn parse_entry(&mut self) -> Result<(BspNode, f32, Option<String>, Option<String>), String> {
        self.skip_ws();
        let id = self.parse_identifier()?;

        let is_keyword = matches!(id.as_str(), "line" | "col" | "tabs");

        self.skip_ws();
        if !is_keyword && self.peek() == Some(b':') {
            self.pos += 1;
            self.skip_ws();
            let node_id = self.parse_identifier()?;
            let is_kw = matches!(node_id.as_str(), "line" | "col" | "tabs");
            if is_kw {
                let node = self.parse_split(&node_id)?;
                self.skip_ws();
                let weight = self.try_parse_number_at_boundary().unwrap_or(1.0);
                return Ok((node, weight, Some(id), None));
            }
            self.skip_ws();
            let weight = self.try_parse_number_at_boundary().unwrap_or(1.0);
            let command = self.try_parse_command();
            return Ok((BspNode::Leaf { tag: node_id, command }, weight, Some(id), None));
        }

        if is_keyword {
            let node = self.parse_split(&id)?;
            self.skip_ws();
            let weight = self.try_parse_number_at_boundary().unwrap_or(1.0);
            return Ok((node, weight, None, None));
        }

        self.skip_ws();
        let weight = self.try_parse_number_at_boundary().unwrap_or(1.0);
        let command = self.try_parse_command();
        Ok((BspNode::Leaf { tag: id, command }, weight, None, None))
    }

    fn parse_split(&mut self, keyword: &str) -> Result<BspNode, String> {
        let direction = match keyword {
            "line" => Direction::Horizontal,
            "col" => Direction::Vertical,
            "tabs" => Direction::Tabs,
            _ => return Err(format!("unknown keyword: {keyword}")),
        };
        self.expect(b'(')?;
        let mut children = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b')') {
                self.pos += 1;
                break;
            }
            if !children.is_empty() {
                self.expect(b',')?;
            }
            let (node, weight, label, _) = self.parse_entry()?;
            children.push(BspChild { node, weight, label });
            self.skip_ws();
        }
        if children.len() < 2 && direction != Direction::Tabs {
            return Err("split must have at least 2 children".into());
        }
        Ok(BspNode::Split { direction, children })
    }

    fn try_parse_number_at_boundary(&mut self) -> Option<f32> {
        let saved = self.pos;
        self.skip_ws();
        if let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                return self.parse_number();
            }
        }
        self.pos = saved;
        None
    }

    fn try_parse_command(&mut self) -> Option<String> {
        self.skip_ws();
        if self.peek() == Some(b'=') {
            self.pos += 1;
            self.parse_identifier().ok()
        } else {
            None
        }
    }
}
