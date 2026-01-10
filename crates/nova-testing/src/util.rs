use std::path::Path;

pub(crate) fn rel_path_string(project_root: &Path, path: &Path) -> String {
    let rel = path.strip_prefix(project_root).unwrap_or(path);
    pathbuf_to_slash_string(rel)
}

fn pathbuf_to_slash_string(path: &Path) -> String {
    let mut out = String::new();
    for (idx, component) in path.components().enumerate() {
        if idx > 0 {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    out
}
