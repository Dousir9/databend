// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fs::File;
use std::io::BufReader;

use common_exception::ErrorCode;
use common_exception::Result;
use rustls::Certificate;
use rustls::PrivateKey;
use rustls::ServerConfig;
use rustls_pemfile::certs;
use rustls_pemfile::pkcs8_private_keys;
use rustls_pemfile::rsa_private_keys;

#[derive(Default)]
pub struct MySQLTlsConfig {
    cert_path: String,
    key_path: String,
}

impl MySQLTlsConfig {
    pub fn new(cert_path: String, key_path: String) -> Self {
        Self {
            cert_path,
            key_path,
        }
    }

    fn enabled(&self) -> bool {
        !self.cert_path.is_empty() && !self.key_path.is_empty()
    }

    pub fn setup(&self) -> Result<Option<ServerConfig>> {
        if !self.enabled() {
            return Ok(None);
        }

        let cert = certs(&mut BufReader::new(File::open(&self.cert_path)?))
            .map_err(|err| ErrorCode::TLSConfigurationFailure(err.to_string()))
            .map(|mut certs| certs.drain(..).map(Certificate).collect())?;

        let key = {
            let mut pkcs8 = pkcs8_private_keys(&mut BufReader::new(File::open(&self.key_path)?))
                .map_err(|err| ErrorCode::TLSConfigurationFailure(err.to_string()))?;
            if !pkcs8.is_empty() {
                PrivateKey(pkcs8.remove(0))
            } else {
                let mut rsa = rsa_private_keys(&mut BufReader::new(File::open(&self.key_path)?))
                    .map_err(|err| ErrorCode::TLSConfigurationFailure(err.to_string()))?;
                if !rsa.is_empty() {
                    PrivateKey(rsa.remove(0))
                } else {
                    return Err(ErrorCode::TLSConfigurationFailure(
                        "invalid key".to_string(),
                    ));
                }
            }
        };

        let config = ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(cert, key)
            .map_err(|err| ErrorCode::TLSConfigurationFailure(err.to_string()))?;

        Ok(Some(config))
    }
}
