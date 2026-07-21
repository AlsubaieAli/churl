use super::{Args, ImportError, Parser};
use crate::model::{Part, PartValue};

impl Parser {
    /// Handles one `--flag` (with an optional inline `=value`).
    pub(super) fn long_flag(
        &mut self,
        name: &str,
        inline_value: Option<String>,
        args: &mut Args,
    ) -> Result<(), ImportError> {
        let value = |parser_args: &mut Args| -> Result<String, ImportError> {
            match inline_value.clone() {
                Some(value) => Ok(value),
                None => parser_args
                    .next()
                    .ok_or_else(|| ImportError::MissingValue(format!("--{name}"))),
            }
        };
        match name {
            "request" => self.set_method(&value(args)?),
            "header" => {
                self.add_header(&value(args)?);
                Ok(())
            }
            "data" | "data-raw" | "data-ascii" | "data-binary" => self.add_data(value(args)?),
            "json" => self.add_json(value(args)?),
            "form" => self.add_form(value(args)?),
            "user" => {
                self.add_basic_auth(&value(args)?);
                Ok(())
            }
            "url" => self.set_url(value(args)?),
            "proxy" => {
                let proxy = value(args)?;
                self.note_proxy(&proxy);
                Ok(())
            }
            "output" => {
                let file = value(args)?;
                self.warnings
                    .push(format!("ignored: -o output file {file:?} discarded"));
                Ok(())
            }
            // Output/verbosity flags with no request semantics: accepted silently.
            // `--location` matches behaviour — the reqwest client already follows
            // redirects by default.
            "location" | "silent" | "verbose" | "show-error" => Ok(()),
            "compressed" => {
                self.warnings
                    .push("ignored: compression negotiation not configured".to_owned());
                Ok(())
            }
            "insecure" => {
                self.note_insecure();
                Ok(())
            }
            _ => Err(ImportError::UnknownFlag(format!("--{name}"))),
        }
    }

    /// Handles one short token: a single flag (`-X`), a cluster of value-less
    /// flags (`-sSL`), or a flag with its value attached (`-XPOST`). A
    /// value-taking flag consumes the rest of the token (or the next argument)
    /// and ends the cluster, matching curl.
    pub(super) fn short_cluster(
        &mut self,
        token: &str,
        args: &mut Args,
    ) -> Result<(), ImportError> {
        let chars: Vec<char> = token[1..].chars().collect();
        for (index, &c) in chars.iter().enumerate() {
            match c {
                // Value-less flags: -L follows-redirects (already client default),
                // -s/-v/-S are output/verbosity noise.
                'L' | 's' | 'v' | 'S' => {}
                'k' => self.note_insecure(),
                // `-x` (proxy) joins the value-taking cluster: it consumes the
                // rest of the token or the next argument, like `-X`/`-H`/`-d`.
                'X' | 'H' | 'd' | 'u' | 'o' | 'F' | 'x' => {
                    let rest: String = chars[index + 1..].iter().collect();
                    let value = if rest.is_empty() {
                        args.next()
                            .ok_or_else(|| ImportError::MissingValue(format!("-{c}")))?
                    } else {
                        rest
                    };
                    return match c {
                        'X' => self.set_method(&value),
                        'H' => {
                            self.add_header(&value);
                            Ok(())
                        }
                        'd' => self.add_data(value),
                        'u' => {
                            self.add_basic_auth(&value);
                            Ok(())
                        }
                        'x' => {
                            self.note_proxy(&value);
                            Ok(())
                        }
                        'o' => {
                            self.warnings
                                .push(format!("ignored: -o output file {value:?} discarded"));
                            Ok(())
                        }
                        'F' => self.add_form(value),
                        _ => unreachable!("outer match already narrowed the flag"),
                    };
                }
                _ => return Err(ImportError::UnknownFlag(format!("-{c}"))),
            }
        }
        Ok(())
    }

    /// Records a `-x`/`--proxy` import note. Proxy is not a per-endpoint property,
    /// so it is never persisted onto the imported endpoint — the note points the
    /// user at where it *is* set (session-scoped).
    fn note_proxy(&mut self, proxy: &str) {
        // Mask any `user:pass@` — a proxy note must never echo credentials.
        let shown = crate::config::mask_proxy(proxy);
        self.warnings.push(format!(
            "proxy {shown:?} — not stored per-endpoint; set it via --proxy or the Options overlay"
        ));
    }

    /// Handles `-k`/`--insecure`: bakes durable insecure-TLS onto the imported
    /// endpoint and warns loudly, since it turns off certificate verification for
    /// every send of this endpoint (unlike the session-scoped `-x` proxy).
    fn note_insecure(&mut self) {
        self.insecure = true;
        self.warnings.push(
            "imported with -k: TLS certificate verification is OFF for this endpoint (saved)"
                .to_owned(),
        );
    }

    fn set_method(&mut self, value: &str) -> Result<(), ImportError> {
        self.method = Some(
            value
                .parse()
                .map_err(|_| ImportError::InvalidMethod(value.to_owned()))?,
        );
        Ok(())
    }

    fn add_data(&mut self, value: String) -> Result<(), ImportError> {
        if value.starts_with('@') {
            return Err(ImportError::Unsupported("@file body".to_owned()));
        }
        self.data_parts.push(value);
        Ok(())
    }

    fn add_json(&mut self, value: String) -> Result<(), ImportError> {
        self.add_data(value)?;
        self.json = true;
        Ok(())
    }

    /// Parses one `-F`/`--form` value (M8.6): `name=value` for an inline text
    /// part, or `name=@path` for a file part, with optional curl `;filename=`
    /// / `;type=` modifiers on the `@path` form (either order, unknown
    /// modifiers ignored — churl models only those two). The `@path` string is
    /// captured as-is and **never opened** here — reading happens only at
    /// send time (`crate::http`), preserving "import never reads files".
    /// Mixing `-F` with `-d`/`--data*`/`--json` is validated once, in
    /// [`Parser::finish`] (a single check covers both flag orders).
    fn add_form(&mut self, raw: String) -> Result<(), ImportError> {
        let (name, rest) = raw
            .split_once('=')
            .ok_or_else(|| ImportError::Unsupported(format!("malformed -F value: {raw:?}")))?;
        let mut segments = rest.split(';');
        // curl allows `<@path` too (embed a file's content as the value) —
        // out of scope: that WOULD require reading the file at import time,
        // which churl's import contract forbids. Reject it explicitly rather
        // than silently mis-storing the literal `<@path` string as text.
        let primary = segments.next().unwrap_or("");
        if primary.starts_with('<') {
            return Err(ImportError::Unsupported(
                "-F name=<@file (embed file content) — import never reads files".to_owned(),
            ));
        }
        let mut filename = None;
        let mut mime = None;
        for modifier in segments {
            if let Some(value) = modifier.strip_prefix("filename=") {
                filename = Some(value.to_owned());
            } else if let Some(value) = modifier.strip_prefix("type=") {
                mime = Some(value.to_owned());
            }
            // Other curl -F modifiers (`headers=`, `encoder=`, …) have no
            // churl model equivalent yet and are silently ignored, like other
            // accepted-but-unmapped curl minutiae elsewhere in this importer.
        }
        let value = if let Some(path) = primary.strip_prefix('@') {
            PartValue::File {
                path: path.to_owned(),
                filename,
                mime,
            }
        } else {
            PartValue::Text(primary.to_owned())
        };
        self.parts.push(Part {
            name: name.to_owned(),
            value,
        });
        Ok(())
    }
}
