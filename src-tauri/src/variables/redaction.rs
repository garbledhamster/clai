/// Match before retention so read boundaries and truncation cannot expose a value.
pub(crate) struct ProtectedOutput {
    secrets: Vec<Vec<u8>>,
    pending: Vec<u8>,
    replacement: Option<Vec<u8>>,
}

impl ProtectedOutput {
    pub(crate) fn new(values: Vec<String>) -> Self {
        // A delimiter absent from every payload cannot recreate a secret inside
        // the mask or by joining the text on either side of a replacement.
        let delimiter = std::iter::once('•')
            .chain((0xE000..=0xF8FF).filter_map(char::from_u32))
            .find(|character| values.iter().all(|value| !value.contains(*character)));
        let replacement = delimiter.map(|character| character.to_string().repeat(4).into_bytes());
        let mut secrets = values
            .into_iter()
            .filter(|value| !value.is_empty())
            .map(String::into_bytes)
            .collect::<Vec<_>>();
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        secrets.dedup();
        Self {
            secrets,
            pending: Vec::new(),
            replacement,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8], finished: bool) -> Vec<u8> {
        if self.secrets.is_empty() {
            return bytes.to_vec();
        }
        let Some(replacement) = &self.replacement else {
            return Vec::new();
        };
        self.pending.extend_from_slice(bytes);
        let mut output = Vec::new();
        let mut offset = 0;
        while offset < self.pending.len() {
            let remaining = &self.pending[offset..];
            // Keep a possible longer match until the next read or actual EOF.
            if !finished
                && self
                    .secrets
                    .iter()
                    .any(|secret| secret.len() > remaining.len() && secret.starts_with(remaining))
            {
                break;
            }
            if let Some(secret) = self
                .secrets
                .iter()
                .find(|secret| remaining.starts_with(secret))
            {
                output.extend_from_slice(replacement);
                offset += secret.len();
            } else {
                output.push(self.pending[offset]);
                offset += 1;
            }
        }
        self.pending.drain(..offset);
        output
    }
}
