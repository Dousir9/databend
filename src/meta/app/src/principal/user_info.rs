// Copyright 2021 Datafuse Labs.
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

use core::fmt;
use std::convert::TryFrom;

use common_exception::ErrorCode;
use common_exception::Result;
use enumflags2::bitflags;
use enumflags2::BitFlags;
use serde::Deserialize;
use serde::Serialize;

use crate::principal::AuthInfo;
use crate::principal::UserGrantSet;
use crate::principal::UserIdentity;
use crate::principal::UserQuota;

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq, Default)]
#[serde(default)]
pub struct UserInfo {
    pub name: String,

    pub hostname: String,

    pub auth_info: AuthInfo,

    pub grants: UserGrantSet,

    pub quota: UserQuota,

    pub option: UserOption,
}

impl UserInfo {
    pub fn new(name: &str, hostname: &str, auth_info: AuthInfo) -> Self {
        // Default is no privileges.
        let grants = UserGrantSet::default();
        let quota = UserQuota::no_limit();
        let option = UserOption::default();

        UserInfo {
            name: name.to_string(),
            hostname: hostname.to_string(),
            auth_info,
            grants,
            quota,
            option,
        }
    }

    pub fn new_no_auth(name: &str, hostname: &str) -> Self {
        UserInfo::new(name, hostname, AuthInfo::None)
    }

    pub fn identity(&self) -> UserIdentity {
        UserIdentity {
            username: self.name.clone(),
            hostname: self.hostname.clone(),
        }
    }

    pub fn has_option_flag(&self, flag: UserOptionFlag) -> bool {
        self.option.has_option_flag(flag)
    }

    pub fn update_auth_option(&mut self, auth: Option<AuthInfo>, option: Option<UserOption>) {
        if let Some(auth_info) = auth {
            self.auth_info = auth_info;
        };
        if let Some(user_option) = option {
            self.option = user_option;
        };
    }
}

impl TryFrom<Vec<u8>> for UserInfo {
    type Error = ErrorCode;

    fn try_from(value: Vec<u8>) -> Result<Self> {
        match serde_json::from_slice(&value) {
            Ok(user_info) => Ok(user_info),
            Err(serialize_error) => Err(ErrorCode::IllegalUserInfoFormat(format!(
                "Cannot deserialize user info from bytes. cause {}",
                serialize_error
            ))),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Eq, PartialEq, Default)]
#[serde(default)]
pub struct UserOption {
    flags: BitFlags<UserOptionFlag>,

    default_role: Option<String>,
}

impl UserOption {
    pub fn new(flags: BitFlags<UserOptionFlag>) -> Self {
        Self {
            flags,
            default_role: None,
        }
    }

    pub fn empty() -> Self {
        Default::default()
    }

    pub fn with_flags(mut self, flags: BitFlags<UserOptionFlag>) -> Self {
        self.flags = flags;
        self
    }

    pub fn with_default_role(mut self, default_role: Option<String>) -> Self {
        self.default_role = default_role;
        self
    }

    pub fn with_set_flag(mut self, flag: UserOptionFlag) -> Self {
        self.flags.insert(flag);
        self
    }

    pub fn flags(&self) -> &BitFlags<UserOptionFlag> {
        &self.flags
    }

    pub fn default_role(&self) -> Option<&String> {
        self.default_role.as_ref()
    }

    pub fn set_default_role(&mut self, default_role: Option<String>) {
        self.default_role = default_role;
    }

    pub fn set_all_flag(&mut self) {
        self.flags = BitFlags::all();
    }

    pub fn set_option_flag(&mut self, flag: UserOptionFlag) {
        self.flags.insert(flag);
    }

    pub fn switch_option_flag(&mut self, flag: UserOptionFlag, on: bool) {
        if on {
            self.flags.insert(flag);
        } else {
            self.flags.remove(flag);
        }
    }

    pub fn unset_option_flag(&mut self, flag: UserOptionFlag) {
        self.flags.remove(flag);
    }

    pub fn has_option_flag(&self, flag: UserOptionFlag) -> bool {
        self.flags.contains(flag)
    }
}

#[bitflags]
#[repr(u64)]
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Eq, PartialEq, num_derive::FromPrimitive)]
pub enum UserOptionFlag {
    TenantSetting = 1 << 0,
}

impl std::fmt::Display for UserOptionFlag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UserOptionFlag::TenantSetting => write!(f, "TENANTSETTING"),
        }
    }
}

#[cfg(test)]
mod tests {
    use enumflags2::BitFlags;

    use crate::principal::AuthInfo;
    use crate::principal::UserInfo;
    use crate::principal::UserOption;

    #[test]
    fn test_user_update_auth_option() -> anyhow::Result<()> {
        let mut u = UserInfo::new("a", "b", AuthInfo::None);

        // None does not take effect
        {
            let mut u2 = u.clone();
            u2.update_auth_option(None, None);
            assert_eq!(u2, u);
        }

        // Some updates the corresponding fields
        {
            u.update_auth_option(Some(AuthInfo::JWT), Some(UserOption::new(BitFlags::all())));
            assert_eq!(AuthInfo::JWT, u.auth_info);
            assert_eq!(BitFlags::all(), u.option.flags);
        }

        Ok(())
    }
}
