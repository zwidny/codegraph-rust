// ABOUTME: Kotlin language AST extractor for code intelligence
// ABOUTME: Extracts classes, objects, functions, imports, and call edges

use codegraph_core::{
    CodeNode, EdgeRelationship, EdgeType, ExtractionResult, Language, Location, NodeId, NodeType,
    Span,
};
use std::collections::HashMap;
use tree_sitter::{Node, Tree, TreeCursor};

/// Kotlin AST extractor.
///
/// Covers the structures that matter for navigation and impact analysis:
/// classes, `object`/`companion object` declarations, functions, imports and calls.
/// Properties and type aliases are intentionally skipped to keep the graph focused.
pub struct KotlinExtractor;

#[derive(Default, Clone)]
struct KotlinContext {
    current_class: Option<String>,
    current_function: Option<NodeId>,
}

impl KotlinExtractor {
    pub fn extract_with_edges(tree: &Tree, content: &str, file_path: &str) -> ExtractionResult {
        let mut collector = KotlinCollector::new(content, file_path);
        let mut cursor = tree.walk();
        collector.walk(&mut cursor, KotlinContext::default());
        ExtractionResult {
            nodes: collector.nodes,
            edges: collector.edges,
        }
    }
}

impl super::LanguageExtractor for KotlinExtractor {
    fn extract_with_edges(tree: &Tree, content: &str, file_path: &str) -> ExtractionResult {
        KotlinExtractor::extract_with_edges(tree, content, file_path)
    }

    fn supported_edge_types() -> &'static [EdgeType] {
        &[EdgeType::Imports, EdgeType::Calls]
    }

    fn language() -> Language {
        Language::Kotlin
    }
}

struct KotlinCollector<'a> {
    content: &'a str,
    file_path: &'a str,
    nodes: Vec<CodeNode>,
    edges: Vec<EdgeRelationship>,
}

impl<'a> KotlinCollector<'a> {
    fn new(content: &'a str, file_path: &'a str) -> Self {
        Self {
            content,
            file_path,
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }

    fn walk(&mut self, cursor: &mut TreeCursor, mut ctx: KotlinContext) {
        let node = cursor.node();

        match node.kind() {
            // `import foo.bar.Baz` / `import foo.bar.*`
            "import" => {
                let text = self.node_text(&node);
                if let Some(path) = import_path(&text) {
                    let mut code = CodeNode::new(
                        format!("import {path}"),
                        Some(NodeType::Import),
                        Some(Language::Kotlin),
                        self.location(&node),
                    )
                    .with_content(text);
                    code.span = Some(self.span_for(&node));
                    code.metadata
                        .attributes
                        .insert("kind".into(), "import".into());
                    code.metadata.attributes.insert("path".into(), path.clone());

                    self.edges.push(EdgeRelationship {
                        from: code.id,
                        to: path.clone(),
                        edge_type: EdgeType::Imports,
                        metadata: {
                            let mut meta = HashMap::new();
                            meta.insert("import_type".to_string(), "kotlin_import".to_string());
                            meta.insert("source_file".to_string(), self.file_path.to_string());
                            meta
                        },
                        span: Some(self.span_for(&node)),
                    });
                    self.nodes.push(code);
                }
            }

            // `class Foo`, `interface Foo`, `enum class Foo`
            "class_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node);
                    let content_text = self.node_text(&node);
                    let (node_type, kind) = if is_interface(&content_text) {
                        (NodeType::Interface, "interface")
                    } else if is_enum(&content_text) {
                        (NodeType::Enum, "enum")
                    } else {
                        (NodeType::Class, "class")
                    };

                    let mut code = CodeNode::new(
                        name.clone(),
                        Some(node_type),
                        Some(Language::Kotlin),
                        self.location(&node),
                    )
                    .with_content(content_text);
                    code.span = Some(self.span_for(&node));
                    code.metadata.attributes.insert("kind".into(), kind.into());
                    code.metadata.attributes.insert(
                        "parent_class".into(),
                        ctx.current_class.clone().unwrap_or_default(),
                    );

                    ctx.current_class = Some(name);
                    self.nodes.push(code);
                }
            }

            // `object Foo`, `companion object`
            "object_declaration" | "companion_object" => {
                let name = node
                    .child_by_field_name("name")
                    .map(|name| self.node_text(&name))
                    .unwrap_or_else(|| "Companion".to_string());
                let kind = if node.kind() == "companion_object" {
                    "companion_object"
                } else {
                    "object"
                };
                let mut code = CodeNode::new(
                    name.clone(),
                    Some(NodeType::Class),
                    Some(Language::Kotlin),
                    self.location(&node),
                )
                .with_content(self.node_text(&node));
                code.span = Some(self.span_for(&node));
                code.metadata.attributes.insert("kind".into(), kind.into());
                code.metadata.attributes.insert(
                    "parent_class".into(),
                    ctx.current_class.clone().unwrap_or_default(),
                );

                ctx.current_class = Some(name);
                self.nodes.push(code);
            }

            // `fun foo(...)` including extension and suspend functions
            "function_declaration" => {
                if let Some(name_node) = node.child_by_field_name("name") {
                    let name = self.node_text(&name_node);
                    let content_text = self.node_text(&node);
                    let mut code = CodeNode::new(
                        name.clone(),
                        Some(NodeType::Function),
                        Some(Language::Kotlin),
                        self.location(&node),
                    )
                    .with_content(content_text.clone())
                    .with_complexity(
                        crate::complexity::calculate_cyclomatic_complexity(&node, self.content),
                    );
                    code.span = Some(self.span_for(&node));
                    code.metadata
                        .attributes
                        .insert("kind".into(), "function".into());
                    if let Some(current_class) = &ctx.current_class {
                        code.metadata
                            .attributes
                            .insert("parent_class".into(), current_class.clone());
                    }
                    if content_text.trim_start().starts_with("suspend ") {
                        code.metadata
                            .attributes
                            .insert("suspend".into(), "true".into());
                    }

                    let id = code.id;
                    self.nodes.push(code);
                    ctx.current_function = Some(id);
                }
            }

            // Calls attributed to the enclosing function.
            "call_expression" => {
                if let Some(current_function) = ctx.current_function {
                    if let Some(callee) = self.callee_name(&node) {
                        self.edges.push(EdgeRelationship {
                            from: current_function,
                            to: callee,
                            edge_type: EdgeType::Calls,
                            metadata: {
                                let mut meta = HashMap::new();
                                meta.insert("call_type".to_string(), "kotlin_call".to_string());
                                meta
                            },
                            span: Some(self.span_for(&node)),
                        });
                    }
                }
            }

            _ => {}
        }

        if cursor.goto_first_child() {
            loop {
                self.walk(cursor, ctx.clone());
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
            cursor.goto_parent();
        }
    }

    /// Best-effort callee name: the last segment of `a.b.c()` or the bare identifier.
    fn callee_name(&self, call: &Node) -> Option<String> {
        let target = call.child(0)?;
        self.text_callee(&target)
    }

    fn text_callee(&self, node: &Node) -> Option<String> {
        let text = self.node_text(node);
        let last = text
            .rsplit('.')
            .next()?
            .split(['<', '(', '[', ' ', '\n', '\t'])
            .next()?
            .trim();
        if last.is_empty() {
            None
        } else {
            Some(last.to_string())
        }
    }

    fn node_text(&self, node: &Node) -> String {
        node.utf8_text(self.content.as_bytes())
            .unwrap_or("")
            .to_string()
    }

    fn span_for(&self, node: &Node) -> Span {
        Span {
            start_byte: node.start_byte() as u32,
            end_byte: node.end_byte() as u32,
        }
    }

    fn location(&self, node: &Node) -> Location {
        Location {
            file_path: self.file_path.to_string(),
            line: (node.start_position().row + 1) as u32,
            column: (node.start_position().column + 1) as u32,
            end_line: Some((node.end_position().row + 1) as u32),
            end_column: Some((node.end_position().column + 1) as u32),
        }
    }
}

fn import_path(text: &str) -> Option<String> {
    let rest = text.trim().strip_prefix("import")?.trim();
    let path = rest.split_whitespace().next()?;
    Some(path.trim_end_matches(';').to_string())
}

fn is_interface(text: &str) -> bool {
    text.split_whitespace().any(|word| word == "interface")
}

fn is_enum(text: &str) -> bool {
    text.split_whitespace().any(|word| word == "enum")
        && text.split_whitespace().any(|word| word == "class")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter::Parser;

    fn parse(content: &str) -> ExtractionResult {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_kotlin_ng::LANGUAGE.into())
            .expect("kotlin grammar");
        let tree = parser.parse(content, None).expect("parse");
        KotlinExtractor::extract_with_edges(&tree, content, "Sample.kt")
    }

    #[test]
    fn extracts_classes_objects_functions_imports_and_calls() {
        let content = r#"
import kotlinx.coroutines.flow.Flow

class Service(val dependency: Dependency) {
    companion object {
        const val NAME = "service"
    }

    suspend fun load(id: String): Result {
        return dependency.fetch(id)
    }
}

interface Port {
    fun send(message: String)
}

object Registry {
    fun register() {}
}
"#;
        let result = parse(content);
        let names: Vec<&str> = result.nodes.iter().map(|node| node.name.as_str()).collect();

        assert!(names.contains(&"Service"));
        assert!(names.contains(&"Port"));
        assert!(names.contains(&"Registry"));
        assert!(names.contains(&"Companion"));
        assert!(names.contains(&"load"));
        assert!(names.contains(&"send"));
        assert!(names.contains(&"register"));
        assert!(names.contains(&"import kotlinx.coroutines.flow.Flow"));

        assert!(
            result
                .edges
                .iter()
                .any(|edge| edge.edge_type == EdgeType::Imports)
        );
        assert!(
            result
                .edges
                .iter()
                .any(|edge| edge.edge_type == EdgeType::Calls && edge.to == "fetch")
        );
    }

    #[test]
    fn enumerates_interface_and_enum_kinds() {
        let content = "enum class Color { RED }\ninterface Marker\n";
        let result = parse(content);
        let kinds: Vec<(String, String)> = result
            .nodes
            .iter()
            .map(|node| {
                (
                    node.name.to_string(),
                    node.metadata
                        .attributes
                        .get("kind")
                        .cloned()
                        .unwrap_or_default(),
                )
            })
            .collect();
        assert!(kinds.contains(&("Color".to_string(), "enum".to_string())));
        assert!(kinds.contains(&("Marker".to_string(), "interface".to_string())));
    }
}
