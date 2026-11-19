//! In-memory edit of GraphicsRules.sgr: gives every `SimQuality` setting the
//! HQ sim texture sizes (4096) so that no file has to be edited by hand.
//! The result always has exactly the length of the original, so it can be
//! laid over the bytes the game reads.

const KEYS: [(&[u8], &[u8], &[u8]); 2] = [
    (b"RenderSimTextureSizes", b"\"4096, 4096, 4096, 4096\"", b"\"4096,4096,4096,4096\""),
    (b"CASSimTextureSize", b"4096", b"4096"),
];

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Rewrite one line (without its line ending) to `<key> <value>`, keeping its
/// length by padding at the end or by giving up leading indentation.
fn rewrite(line: &[u8], at: usize, key: &[u8], value: &[u8]) -> Option<Vec<u8>> {
    let mut new = line[..at + key.len()].to_vec();
    new.push(b' ');
    new.extend_from_slice(value);
    if new.len() > line.len() {
        let excess = new.len() - line.len();
        let indent = new.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
        if indent < excess {
            return None;
        }
        new.drain(..excess);
    }
    new.resize(line.len(), b' ');
    Some(new)
}

/// Returns the edited copy, how many lines were changed and how many already asked for 4096.
pub fn patch(src: &[u8]) -> (Vec<u8>, usize, usize) {
    let mut out = Vec::with_capacity(src.len());
    let (mut changed, mut already, mut in_sim) = (0, 0, false);
    for raw in src.split_inclusive(|b| *b == b'\n') {
        let eol = raw.iter().rev().take_while(|b| **b == b'\n' || **b == b'\r').count();
        let (line, end) = raw.split_at(raw.len() - eol);
        let start = line.iter().take_while(|b| **b == b' ' || **b == b'\t').count();
        let body = &line[start..];
        if body.starts_with(b"option ") || body.starts_with(b"option\t") {
            in_sim = find(body, b"SimQuality").is_some();
        }
        let mut done = false;
        if in_sim && !body.starts_with(b"#") {
            for (key, value, compact) in KEYS {
                let Some(at) = find(line, key) else { continue };
                // the key must be a whole word (not ThumbnailSim..., not ...Sizes vs ...Size)
                let next = line.get(at + key.len()).copied().unwrap_or(b' ');
                let prev = if at == 0 { b' ' } else { line[at - 1] };
                if next.is_ascii_alphanumeric() || prev.is_ascii_alphanumeric() {
                    continue;
                }
                if find(&line[at..], b"4096").is_some() {
                    already += 1;
                } else if let Some(new) = rewrite(line, at, key, value).or_else(|| rewrite(line, at, key, compact)) {
                    out.extend_from_slice(&new);
                    changed += 1;
                    done = true;
                }
                break;
            }
        }
        if !done {
            out.extend_from_slice(line);
        }
        out.extend_from_slice(end);
    }
    if out.len() != src.len() {
        return (src.to_vec(), 0, already); // cannot happen; never hand out a wrong length
    }
    (out, changed, already)
}
