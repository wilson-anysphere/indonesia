pub(crate) use nova_framework_parse::{
    clean_type, collect_annotations, find_named_child, modifier_node, node_text, parse_java,
    simple_name, visit_nodes, ParsedAnnotation,
};

use tree_sitter::Node;

pub(crate) fn infer_field_type_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            k if k == "modifiers" || k.ends_with("annotation") => continue,
            "variable_declarator" => break,
            _ => return Some(child),
        }
    }
    None
}

pub(crate) fn infer_param_type_node<'a>(node: Node<'a>) -> Option<Node<'a>> {
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        match child.kind() {
            k if k == "modifiers" || k.ends_with("annotation") => continue,
            // Parameter name.
            "identifier" => break,
            _ => return Some(child),
        }
    }
    None
}
