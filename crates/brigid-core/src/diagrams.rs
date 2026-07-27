//! Deterministic index diagram builders (minimal M1 surface).
//!
//! Builds sanitized Mermaid for index maps from module inventory. Full index
//! combine remains M4; these helpers only emit structural flowcharts and a
//! simple learning-path diagram. All output is passed through
//! [`crate::mermaid::sanitize_mermaid`] + [`crate::mermaid::validate_mermaid`].

use crate::mermaid::{sanitize_label, sanitize_mermaid, stable_node_id, validate_mermaid};
use crate::module::{ModuleCount, ModuleKey, ROOT_MODULE};

/// A directed edge between two module keys (for system maps).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagramEdge {
    /// Source module key (as path-like string, e.g. `apps/alpha`).
    pub from: String,
    /// Target module key.
    pub to: String,
    /// Optional edge label (sanitized when rendered).
    pub label: Option<String>,
}

/// Build a module/app inventory flowchart from module keys.
///
/// Emits a top-down flowchart with one node per key (stable ids `M0`, `M1`, …)
/// ordered as provided. Always sanitized/validated; panics never — if
/// validation fails after sanitize (should be rare), returns a stub diagram.
///
/// # Examples
///
/// ```
/// use brigid_core::diagrams::module_inventory_flowchart;
/// use brigid_core::module::ModuleKey;
///
/// let keys = [ModuleKey::new("apps/alpha"), ModuleKey::new("_root")];
/// let mermaid = module_inventory_flowchart(&keys);
/// assert!(mermaid.contains("flowchart"));
/// assert!(mermaid.contains("apps/alpha") || mermaid.contains("apps alpha"));
/// ```
#[must_use]
pub fn module_inventory_flowchart(modules: &[ModuleKey]) -> String {
    let mut body = String::from("flowchart TD\n");
    body.push_str("  ROOT[Repository]\n");
    for (i, key) in modules.iter().enumerate() {
        let id = stable_node_id("M", i);
        let label = display_module_label(key.as_str());
        body.push_str(&format!("  {id}[{label}]\n"));
        body.push_str(&format!("  ROOT --> {id}\n"));
    }
    finalize_diagram(&body)
}

/// Build a module inventory flowchart from [`ModuleCount`] rows (uses keys only).
#[must_use]
pub fn module_inventory_from_counts(modules: &[ModuleCount]) -> String {
    let keys: Vec<ModuleKey> = modules.iter().map(|m| m.key.clone()).collect();
    module_inventory_flowchart(&keys)
}

/// Build a system map flowchart: module nodes plus optional edges.
///
/// Nodes are created for every module that appears in `modules` or in an edge
/// endpoint. Edges are rendered as `A --> B` or `A -->|label| B`.
///
/// # Examples
///
/// ```
/// use brigid_core::diagrams::{DiagramEdge, system_map_flowchart};
/// use brigid_core::module::ModuleKey;
///
/// let modules = [ModuleKey::new("apps/api"), ModuleKey::new("apps/web")];
/// let edges = [DiagramEdge {
///     from: "apps/web".into(),
///     to: "apps/api".into(),
///     label: Some("HTTP".into()),
/// }];
/// let mermaid = system_map_flowchart(&modules, &edges);
/// assert!(mermaid.contains("flowchart"));
/// ```
#[must_use]
pub fn system_map_flowchart(modules: &[ModuleKey], edges: &[DiagramEdge]) -> String {
    // Collect unique keys in stable order: modules first, then edge endpoints.
    let mut keys: Vec<String> = modules.iter().map(|k| k.as_str().to_owned()).collect();
    for e in edges {
        if !keys.iter().any(|k| k == &e.from) {
            keys.push(e.from.clone());
        }
        if !keys.iter().any(|k| k == &e.to) {
            keys.push(e.to.clone());
        }
    }

    let mut body = String::from("flowchart LR\n");
    let mut id_for: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    for (i, key) in keys.iter().enumerate() {
        let id = stable_node_id("N", i);
        id_for.insert(key.clone(), id.clone());
        let label = display_module_label(key);
        body.push_str(&format!("  {id}[{label}]\n"));
    }
    for e in edges {
        let from = id_for
            .get(&e.from)
            .cloned()
            .unwrap_or_else(|| stable_node_id("X", 0));
        let to = id_for
            .get(&e.to)
            .cloned()
            .unwrap_or_else(|| stable_node_id("X", 1));
        match &e.label {
            Some(lbl) if !lbl.is_empty() => {
                let safe = sanitize_label(lbl);
                body.push_str(&format!("  {from} -->|{safe}| {to}\n"));
            }
            _ => body.push_str(&format!("  {from} --> {to}\n")),
        }
    }
    finalize_diagram(&body)
}

/// Build a simple learning-path flowchart from ordered chapter/step titles.
///
/// Steps are linked in order: `S0 --> S1 --> …`. Titles are sanitized labels.
///
/// # Examples
///
/// ```
/// use brigid_core::diagrams::learning_path_flowchart;
///
/// let steps = ["Setup", "Core concepts", "Advanced"];
/// let mermaid = learning_path_flowchart(&steps);
/// assert!(mermaid.contains("flowchart"));
/// assert!(mermaid.contains("Setup"));
/// ```
#[must_use]
pub fn learning_path_flowchart(steps: &[&str]) -> String {
    let mut body = String::from("flowchart TD\n");
    if steps.is_empty() {
        body.push_str("  S0[No steps]\n");
        return finalize_diagram(&body);
    }
    for (i, step) in steps.iter().enumerate() {
        let id = stable_node_id("S", i);
        let label = sanitize_label(step);
        let label = if label.is_empty() {
            format!("Step {i}")
        } else {
            label
        };
        body.push_str(&format!("  {id}[{label}]\n"));
        if i > 0 {
            let prev = stable_node_id("S", i - 1);
            body.push_str(&format!("  {prev} --> {id}\n"));
        }
    }
    finalize_diagram(&body)
}

fn display_module_label(key: &str) -> String {
    if key == ROOT_MODULE {
        return sanitize_label("root");
    }
    // Prefer readable form without raw path noise.
    sanitize_label(&key.replace('/', " "))
}

fn finalize_diagram(raw: &str) -> String {
    let sanitized = sanitize_mermaid(raw);
    if validate_mermaid(&sanitized).valid {
        sanitized
    } else {
        // Deterministic stub — should be rare given our builders.
        sanitize_mermaid("flowchart TD\n  X0[Diagram unavailable]\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::ModuleKey;

    #[test]
    fn module_inventory_is_valid_and_deterministic() {
        let keys = [
            ModuleKey::new("apps/alpha"),
            ModuleKey::new("apps/beta"),
            ModuleKey::new("_root"),
        ];
        let a = module_inventory_flowchart(&keys);
        let b = module_inventory_flowchart(&keys);
        assert_eq!(a, b);
        assert!(a.starts_with("flowchart"));
        assert!(validate_mermaid(&a).valid, "{a}");
        assert!(a.contains("M0"));
        assert!(a.contains("ROOT"));
        // No forbidden chars
        assert!(!a.contains('"'));
        assert!(!a.contains('#'));
    }

    #[test]
    fn empty_inventory_still_valid() {
        let m = module_inventory_flowchart(&[]);
        assert!(validate_mermaid(&m).valid, "{m}");
        assert!(m.contains("ROOT"));
    }

    #[test]
    fn system_map_with_edges() {
        let modules = [ModuleKey::new("apps/api"), ModuleKey::new("apps/web")];
        let edges = [DiagramEdge {
            from: "apps/web".into(),
            to: "apps/api".into(),
            label: Some(r#"HTTP "call""#.into()),
        }];
        let m = system_map_flowchart(&modules, &edges);
        assert!(validate_mermaid(&m).valid, "{m}");
        assert!(!m.contains('"'), "{m}");
        assert!(m.contains("N0") || m.contains("N1"), "{m}");
    }

    #[test]
    fn learning_path_links_in_order() {
        let m = learning_path_flowchart(&["Setup", "Overview", "Deep dive"]);
        assert!(validate_mermaid(&m).valid, "{m}");
        assert!(m.contains("S0"));
        assert!(m.contains("S1"));
        assert!(m.contains("S0 --> S1"), "{m}");
        assert!(m.contains("Setup"));
    }

    #[test]
    fn learning_path_empty() {
        let m = learning_path_flowchart(&[]);
        assert!(validate_mermaid(&m).valid, "{m}");
    }

    #[test]
    fn inventory_from_counts_matches_keys() {
        let counts = vec![
            ModuleCount {
                key: ModuleKey::new("apps/a"),
                count: 2,
            },
            ModuleCount {
                key: ModuleKey::new("_root"),
                count: 1,
            },
        ];
        let from_counts = module_inventory_from_counts(&counts);
        let from_keys =
            module_inventory_flowchart(&[ModuleKey::new("apps/a"), ModuleKey::new("_root")]);
        assert_eq!(from_counts, from_keys);
    }
}
