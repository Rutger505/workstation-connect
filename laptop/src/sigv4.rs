//! AWS Signature Version 4 (header auth, signed payload), just enough for an S3 PUT.
//! https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

#[derive(Clone, PartialEq)]
pub struct Credentials {
    pub access: String,
    pub secret: String,
}

/// Redacted: credentials must never end up in logs (which get uploaded).
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access", &"<redacted>")
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// What gets signed. `headers`: lowercase names, trimmed values, must include `host` and
/// `x-amz-date`; `uri`: already encoded ([`encode_path`]); `query`: canonical, usually empty.
pub struct Request<'a> {
    pub method: &'a str,
    pub uri: &'a str,
    pub query: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_sha256: &'a str,
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("hmac takes any key length");
    m.update(data.as_bytes());
    m.finalize().into_bytes().to_vec()
}

/// S3 path encoding: every byte except unreserved and '/'.
pub fn encode_path(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `Authorization` header value. `amz_date`: YYYYMMDDTHHMMSSZ, same as the x-amz-date header.
pub fn authorization(
    creds: &Credentials,
    region: &str,
    service: &str,
    amz_date: &str,
    req: &Request,
) -> String {
    let mut headers: Vec<_> = req.headers.to_vec();
    headers.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_headers: String = headers.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let signed = headers
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}",
        req.method, req.uri, req.query, req.payload_sha256
    );
    let date = &amz_date[..8];
    let scope = format!("{date}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical.as_bytes())
    );
    let mut key = hmac(format!("AWS4{}", creds.secret).as_bytes(), date);
    for part in [region, service, "aws4_request"] {
        key = hmac(&key, part);
    }
    let signature = hex::encode(hmac(&key, &to_sign));
    format!(
        "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        creds.access
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// AWS SigV4 test suite: get-vanilla.
    #[test]
    fn aws_test_suite_get_vanilla() {
        let creds = Credentials {
            access: "AKIDEXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
        };
        let auth = authorization(
            &creds,
            "us-east-1",
            "service",
            "20150830T123600Z",
            &Request {
                method: "GET",
                uri: "/",
                query: "",
                headers: &[
                    ("host", "example.amazonaws.com"),
                    ("x-amz-date", "20150830T123600Z"),
                ],
                payload_sha256: EMPTY,
            },
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, \
             SignedHeaders=host;x-amz-date, \
             Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
    }

    /// S3 docs example "PUT Object" (sig-v4-header-based-auth).
    #[test]
    fn s3_docs_put_object() {
        let creds = Credentials {
            access: "AKIAIOSFODNN7EXAMPLE".into(),
            secret: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        };
        let body = b"Welcome to Amazon S3.";
        let hash = sha256_hex(body);
        assert_eq!(
            hash,
            "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072"
        );
        let auth = authorization(
            &creds,
            "us-east-1",
            "s3",
            "20130524T000000Z",
            &Request {
                method: "PUT",
                uri: &encode_path("/test$file.text"),
                query: "",
                headers: &[
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                    ("x-amz-date", "20130524T000000Z"),
                    ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                    ("x-amz-content-sha256", &hash),
                ],
                payload_sha256: &hash,
            },
        );
        assert!(
            auth.ends_with(
                "SignedHeaders=date;host;x-amz-content-sha256;x-amz-date;x-amz-storage-class, \
                 Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
            ),
            "{auth}"
        );
    }

    #[test]
    fn debug_redacts_keys() {
        let c = Credentials {
            access: "AKIDEXAMPLE".into(),
            secret: "wJalrXUtnFEMI".into(),
        };
        let d = format!("{c:?}");
        assert!(
            !d.contains("AKIDEXAMPLE") && !d.contains("wJalrXUtnFEMI"),
            "{d}"
        );
    }

    #[test]
    fn encodes_paths() {
        assert_eq!(
            encode_path("/b/laptop/my host/x+y.log"),
            "/b/laptop/my%20host/x%2By.log"
        );
    }
}
