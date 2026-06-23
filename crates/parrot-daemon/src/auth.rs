use rand::Rng;
use std::path::Path;
use tokio::fs;
use tracing::info;

pub struct Auth {
    token: String,
}

impl Auth {
    pub async fn new(token_path: &Path) -> Result<Self, std::io::Error> {
        let token = if token_path.exists() {
            let token = fs::read_to_string(token_path).await?;
            token.trim().to_string()
        } else {
            let token = Self::generate_token();
            if let Some(parent) = token_path.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::write(token_path, &token).await?;
            // Restrict the token file to owner-only (0600 on Unix). Required
            // by the security model (spec §8): the token gates access to the
            // daemon, so it must not be world/group-readable on multi-user
            // systems. On Windows this is a no-op (ACLs govern access there).
            restrict_token_file_permissions(token_path)?;
            info!("Generated new auth token at {:?}", token_path);
            token
        };

        Ok(Self { token })
    }

    pub fn validate(&self, token: &str) -> bool {
        self.token == token
    }

    fn generate_token() -> String {
        let mut rng = rand::rng();
        let bytes: [u8; 32] = rng.random();
        hex::encode(bytes)
    }
}

#[cfg(unix)]
fn restrict_token_file_permissions(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_token_file_permissions(_path: &Path) -> Result<(), std::io::Error> {
    // Windows uses ACLs, not mode bits. The file is created in the user's
    // profile dir which is already owner-restricted by default on modern NTFS.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_token_is_hex() {
        let token = Auth::generate_token();
        assert_eq!(token.len(), 64); // 32 bytes = 64 hex chars
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_validate_correct_token() {
        let token = Auth::generate_token();
        let auth = Auth {
            token: token.clone(),
        };
        assert!(auth.validate(&token));
    }

    #[test]
    fn test_validate_wrong_token() {
        let auth = Auth {
            token: "abc123".to_string(),
        };
        assert!(!auth.validate("wrong_token"));
    }

    #[tokio::test]
    async fn test_auth_new_creates_token_file() {
        let dir = std::env::temp_dir().join("parrot_test_auth_new");
        let _ = std::fs::remove_dir_all(&dir);
        let token_path = dir.join("token");

        let auth = Auth::new(&token_path).await.unwrap();
        assert!(!auth.token.is_empty());
        assert!(token_path.exists());

        // Loading again should give same token
        let auth2 = Auth::new(&token_path).await.unwrap();
        assert_eq!(auth.token, auth2.token);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
