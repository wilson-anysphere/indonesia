use std::collections::{HashMap, HashSet};

use nova_types::{Diagnostic, Severity};
use regex::Regex;

pub const CDI_UNSATISFIED_CODE: &str = "QUARKUS_CDI_UNSATISFIED_DEPENDENCY";
pub const CDI_AMBIGUOUS_CODE: &str = "QUARKUS_CDI_AMBIGUOUS_DEPENDENCY";
pub const CDI_CIRCULAR_CODE: &str = "QUARKUS_CDI_CIRCULAR_DEPENDENCY";

#[derive(Debug, Clone)]
pub struct CdiAnalysis {
    pub model: CdiModel,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug, Clone, Default)]
pub struct CdiModel {
    pub beans: Vec<CdiBean>,
    pub injection_points: Vec<CdiInjectionPoint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeanKind {
    Class,
    ProducerMethod,
}

#[derive(Debug, Clone)]
pub struct CdiBean {
    pub name: String,
    pub kind: BeanKind,
    pub provided_types: Vec<String>,
    pub qualifiers: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct CdiInjectionPoint {
    pub required_type: String,
    pub qualifiers: Vec<String>,
}

#[derive(Clone, Debug)]
struct Bean {
    name: String,
    provided_types: HashSet<String>,
    qualifiers: Qualifiers,
    kind: BeanKind,
    dependencies: Vec<InjectionPoint>,
}

#[derive(Clone, Debug)]
struct InjectionPoint {
    required_type: String,
    qualifiers: Qualifiers,
}

#[derive(Clone, Debug, Default)]
struct Qualifiers {
    named: Option<String>,
    custom: HashSet<String>,
}

impl Qualifiers {
    fn is_empty(&self) -> bool {
        self.named.is_none() && self.custom.is_empty()
    }
}

pub fn analyze_cdi(sources: &[&str]) -> CdiAnalysis {
    let index = build_index(sources);
    let diagnostics = compute_diagnostics(&index);

    let model = CdiModel {
        beans: index
            .beans
            .iter()
            .map(|bean| CdiBean {
                name: bean.name.clone(),
                kind: bean.kind.clone(),
                provided_types: bean.provided_types.iter().cloned().collect(),
                qualifiers: format_qualifiers(&bean.qualifiers),
            })
            .collect(),
        injection_points: index
            .injections
            .iter()
            .map(|ip| CdiInjectionPoint {
                required_type: ip.required_type.clone(),
                qualifiers: format_qualifiers(&ip.qualifiers),
            })
            .collect(),
    };

    CdiAnalysis { model, diagnostics }
}

struct CdiIndex {
    beans: Vec<Bean>,
    injections: Vec<InjectionPoint>,
}

fn build_index(sources: &[&str]) -> CdiIndex {
    let class_re = Regex::new(
        r#"^\s*(?:public|protected|private|abstract|final|static|\s)*\s*(class|interface)\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:extends\s+([A-Za-z0-9_$.<>]+))?\s*(?:implements\s+([^{]+))?\s*\{?"#,
    )
    .unwrap();
    let field_re = Regex::new(
        r#"^\s*(?:(?:public|protected|private|static|final|transient|volatile)\s+)*([^;=]+?)\s+([A-Za-z_][A-Za-z0-9_]*)\s*(?:=|;)"#,
    )
    .unwrap();
    let producer_method_re = Regex::new(
        r#"^\s*(?:(?:public|protected|private|static|final|synchronized|abstract|default)\s+)*([\w<>\[\].$]+)\s+([A-Za-z_][A-Za-z0-9_]*)\s*\("#,
    )
    .unwrap();

    let mut beans = Vec::new();
    let mut injections = Vec::new();

    for src in sources {
        let mut pending_annotations: Vec<(String, Option<String>)> = Vec::new();
        let mut class_name = None::<String>;
        let mut class_qualifiers = Qualifiers::default();
        let mut class_provided_types = HashSet::new();
        let mut class_is_bean = false;
        let mut class_dependencies: Vec<InjectionPoint> = Vec::new();

        let mut brace_depth: i32 = 0;
        let mut in_class = false;

        for raw_line in src.lines() {
            let line_no_comment = raw_line.split("//").next().unwrap_or(raw_line);
            let mut line = line_no_comment;

            line = consume_leading_annotations(line, &mut pending_annotations);
            if line.trim().is_empty() {
                continue;
            }

            if class_name.is_none() {
                if let Some(cap) = class_re.captures(line) {
                    class_name = Some(cap[2].to_string());
                    class_provided_types.insert(cap[2].to_string());
                    if let Some(extends) = cap.get(3) {
                        class_provided_types.insert(simple_name(extends.as_str()));
                    }
                    if let Some(implements) = cap.get(4) {
                        for iface in implements.as_str().split(',') {
                            let iface = iface.trim();
                            if iface.is_empty() {
                                continue;
                            }
                            let iface = iface.split_whitespace().next().unwrap_or(iface);
                            class_provided_types.insert(simple_name(iface));
                        }
                    }

                    class_qualifiers = parse_qualifiers(&pending_annotations);
                    class_is_bean = is_bean_class(&pending_annotations);
                    pending_annotations.clear();
                    in_class = true;
                }
            } else if in_class && brace_depth == 1 {
                let member_qualifiers = parse_qualifiers(&pending_annotations);
                let has_inject = pending_annotations.iter().any(|(n, _)| n == "Inject");
                let has_produces = pending_annotations.iter().any(|(n, _)| n == "Produces");

                if has_inject {
                    if let Some(cap) = field_re.captures(line) {
                        let ty = simple_name(cap[1].trim());
                        let ip = InjectionPoint {
                            required_type: ty,
                            qualifiers: member_qualifiers.clone(),
                        };
                        injections.push(ip.clone());
                        class_dependencies.push(ip);
                    } else if line.contains('(') {
                        for (ty, qualifiers) in parse_parameter_types(line) {
                            let ip = InjectionPoint {
                                required_type: ty,
                                qualifiers,
                            };
                            injections.push(ip.clone());
                            class_dependencies.push(ip);
                        }
                    }
                }

                if has_produces {
                    if let Some(cap) = producer_method_re.captures(line) {
                        let return_ty = simple_name(cap[1].trim());
                        let mut qualifiers = class_qualifiers.clone();
                        qualifiers.named = qualifiers.named.or(member_qualifiers.named);
                        qualifiers.custom.extend(member_qualifiers.custom);
                        beans.push(Bean {
                            name: format!(
                                "{}::{}",
                                class_name.as_deref().unwrap_or("<unknown>"),
                                &cap[2]
                            ),
                            provided_types: HashSet::from([return_ty]),
                            qualifiers,
                            kind: BeanKind::ProducerMethod,
                            dependencies: Vec::new(),
                        });
                    }
                }

                pending_annotations.clear();
            }

            brace_depth += count_braces(line);
        }

        if class_is_bean {
            let name = class_name.unwrap_or_else(|| "<unknown>".to_string());
            beans.push(Bean {
                name,
                provided_types: class_provided_types,
                qualifiers: class_qualifiers,
                kind: BeanKind::Class,
                dependencies: class_dependencies,
            });
        }
    }

    CdiIndex { beans, injections }
}

fn compute_diagnostics(index: &CdiIndex) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    for ip in &index.injections {
        let matches = resolve_injection(index, ip);
        if matches.is_empty() {
            diagnostics.push(Diagnostic::error(
                CDI_UNSATISFIED_CODE,
                format!("Unsatisfied dependency: {}", ip.required_type),
                None,
            ));
        } else if matches.len() > 1 {
            diagnostics.push(Diagnostic::error(
                CDI_AMBIGUOUS_CODE,
                format!(
                    "Ambiguous dependency: {} ({} candidates)",
                    ip.required_type,
                    matches.len()
                ),
                None,
            ));
        }
    }

    diagnostics.extend(detect_circular_dependencies(index));

    diagnostics
}

fn resolve_injection<'a>(index: &'a CdiIndex, ip: &InjectionPoint) -> Vec<&'a Bean> {
    index
        .beans
        .iter()
        .filter(|bean| bean.provided_types.contains(&ip.required_type))
        .filter(|bean| qualifiers_match(&bean.qualifiers, &ip.qualifiers))
        .collect()
}

fn qualifiers_match(bean: &Qualifiers, required: &Qualifiers) -> bool {
    if required.is_empty() {
        // No qualifiers at injection point => @Default.
        // A bean with custom qualifiers (other than @Named) does *not* have @Default.
        return bean.custom.is_empty();
    }

    if let Some(named) = &required.named {
        match &bean.named {
            Some(bn) if bn == named => {}
            Some(_) | None => return false,
        }
    }

    required.custom.is_subset(&bean.custom)
}

fn detect_circular_dependencies(index: &CdiIndex) -> Vec<Diagnostic> {
    let class_beans: Vec<&Bean> = index
        .beans
        .iter()
        .filter(|b| matches!(b.kind, BeanKind::Class))
        .collect();

    let mut bean_by_name: HashMap<&str, usize> = HashMap::new();
    for (idx, bean) in class_beans.iter().enumerate() {
        bean_by_name.insert(bean.name.as_str(), idx);
    }

    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); class_beans.len()];
    for (idx, bean) in class_beans.iter().enumerate() {
        for dep in &bean.dependencies {
            let matches = resolve_injection(index, dep);
            let target = matches
                .into_iter()
                .find(|b| matches!(b.kind, BeanKind::Class));
            if let Some(target) = target {
                if let Some(&target_idx) = bean_by_name.get(target.name.as_str()) {
                    edges[idx].push(target_idx);
                }
            }
        }
    }

    let mut diagnostics = Vec::new();
    let mut state = vec![0u8; class_beans.len()]; // 0=unvisited,1=visiting,2=done
    let mut stack = Vec::new();

    fn dfs(
        node: usize,
        edges: &[Vec<usize>],
        state: &mut [u8],
        stack: &mut Vec<usize>,
        cycles: &mut Vec<Vec<usize>>,
    ) {
        state[node] = 1;
        stack.push(node);

        for &next in &edges[node] {
            if state[next] == 0 {
                dfs(next, edges, state, stack, cycles);
            } else if state[next] == 1 {
                if let Some(pos) = stack.iter().position(|n| *n == next) {
                    cycles.push(stack[pos..].to_vec());
                }
            }
        }

        stack.pop();
        state[node] = 2;
    }

    let mut cycles = Vec::new();
    for node in 0..class_beans.len() {
        if state[node] == 0 {
            dfs(node, &edges, &mut state, &mut stack, &mut cycles);
        }
    }

    let mut seen = HashSet::new();
    for cycle in cycles {
        let canonical = canonical_cycle(&cycle);
        if !seen.insert(canonical.clone()) {
            continue;
        }

        let names: Vec<&str> = canonical
            .iter()
            .map(|idx| class_beans[*idx].name.as_str())
            .collect();
        let msg = format!("Circular dependency detected: {}", names.join(" -> "));

        diagnostics.push(Diagnostic {
            severity: Severity::Warning,
            code: CDI_CIRCULAR_CODE,
            message: msg,
            span: None,
        });
    }

    diagnostics
}

fn canonical_cycle(cycle: &[usize]) -> Vec<usize> {
    if cycle.is_empty() {
        return Vec::new();
    }
    let mut best = cycle.to_vec();
    for start in 0..cycle.len() {
        let rotated: Vec<usize> = cycle[start..]
            .iter()
            .chain(cycle[..start].iter())
            .copied()
            .collect();
        if rotated < best {
            best = rotated;
        }
    }
    best
}

fn parse_qualifiers(annotations: &[(String, Option<String>)]) -> Qualifiers {
    let mut q = Qualifiers::default();
    for (name, args) in annotations {
        match name.as_str() {
            "Named" => {
                q.named = args.as_deref().and_then(extract_first_string_literal);
            }
            n if is_non_qualifier_annotation(n) => {}
            n => {
                q.custom.insert(n.to_string());
            }
        }
    }
    q
}

fn format_qualifiers(qualifiers: &Qualifiers) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(name) = &qualifiers.named {
        out.push(format!("Named({name})"));
    }
    for qual in &qualifiers.custom {
        out.push(qual.clone());
    }
    out
}

fn extract_first_string_literal(args: &str) -> Option<String> {
    let start = args.find('"')?;
    let rest = &args[start + 1..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn is_non_qualifier_annotation(name: &str) -> bool {
    matches!(
        name,
        "Inject"
            | "Produces"
            | "ApplicationScoped"
            | "Singleton"
            | "RequestScoped"
            | "SessionScoped"
            | "Dependent"
            | "Path"
            | "GET"
            | "POST"
            | "PUT"
            | "DELETE"
            | "PATCH"
            | "HEAD"
            | "OPTIONS"
            | "ConfigProperty"
    )
}

fn is_bean_class(annotations: &[(String, Option<String>)]) -> bool {
    annotations.iter().any(|(name, _)| {
        matches!(
            name.as_str(),
            "ApplicationScoped"
                | "Singleton"
                | "RequestScoped"
                | "SessionScoped"
                | "Dependent"
                | "Path"
        )
    })
}

fn parse_parameter_types(signature_line: &str) -> Vec<(String, Qualifiers)> {
    let start = match signature_line.find('(') {
        Some(s) => s,
        None => return Vec::new(),
    };
    let end = match signature_line[start + 1..].find(')') {
        Some(e) => start + 1 + e,
        None => return Vec::new(),
    };
    let params = &signature_line[start + 1..end];
    if params.trim().is_empty() {
        return Vec::new();
    }

    params
        .split(',')
        .filter_map(|raw| {
            let raw = raw.trim();
            if raw.is_empty() {
                return None;
            }

            // Extract inline parameter qualifiers like @Named("foo").
            let mut annotations = Vec::new();
            let mut rest = raw;
            while let Some(stripped) = rest.trim_start().strip_prefix('@') {
                // Consume annotation name.
                let mut chars = stripped.chars();
                let mut name = String::new();
                while let Some(c) = chars.next() {
                    if c.is_alphanumeric() || c == '_' || c == '.' {
                        name.push(c);
                    } else {
                        break;
                    }
                }
                let name_simple = name.rsplit('.').next().unwrap_or(&name).to_string();
                let remaining = &stripped[name.len()..];
                let (args, after) = if remaining.trim_start().starts_with('(') {
                    if let Some(close) = remaining.find(')') {
                        (
                            Some(remaining[1..close].trim().to_string()),
                            &remaining[close + 1..],
                        )
                    } else {
                        (None, remaining)
                    }
                } else {
                    (None, remaining)
                };
                annotations.push((name_simple, args));
                rest = after;
            }

            let qualifiers = parse_qualifiers(&annotations);
            let tokens: Vec<&str> = rest.split_whitespace().collect();
            if tokens.len() < 2 {
                return None;
            }
            let ty = simple_name(tokens[0]);
            Some((ty, qualifiers))
        })
        .collect()
}

fn consume_leading_annotations<'a>(
    mut line: &'a str,
    pending: &mut Vec<(String, Option<String>)>,
) -> &'a str {
    loop {
        let trimmed = line.trim_start();
        if !trimmed.starts_with('@') {
            return line;
        }

        // Find end of annotation token.
        let mut idx = 1usize;
        let bytes = trimmed.as_bytes();
        while idx < bytes.len()
            && (bytes[idx].is_ascii_alphanumeric() || bytes[idx] == b'_' || bytes[idx] == b'.')
        {
            idx += 1;
        }

        let full_name = &trimmed[1..idx];
        let name = full_name
            .rsplit('.')
            .next()
            .unwrap_or(full_name)
            .to_string();
        let mut rest = &trimmed[idx..];

        rest = rest.trim_start();
        let args = if rest.starts_with('(') {
            if let Some(close) = rest.find(')') {
                let inner = rest[1..close].trim();
                rest = &rest[close + 1..];
                if inner.is_empty() {
                    None
                } else {
                    Some(inner.to_string())
                }
            } else {
                None
            }
        } else {
            None
        };

        pending.push((name, args));
        line = rest;

        if line.trim_start().is_empty() {
            return line;
        }
    }
}

fn simple_name(name: &str) -> String {
    let name = name.trim();
    let name = name.split('<').next().unwrap_or(name);
    let name = name.trim_end_matches("[]");
    name.rsplit('.').next().unwrap_or(name).to_string()
}

fn count_braces(line: &str) -> i32 {
    let open = line.chars().filter(|c| *c == '{').count() as i32;
    let close = line.chars().filter(|c| *c == '}').count() as i32;
    open - close
}
