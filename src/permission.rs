use crate::constants::{D7FOR, D7OK, WHY_BOT_TIMED_OUT, WHY_TARGET_IS_OWNER};

pub const BIT_KICK_MEMBERS: u32 = 1;
pub const BIT_BAN_MEMBERS: u32 = 2;
pub const BIT_ADMINISTRATOR: u32 = 3;
pub const BIT_MANAGE_GUILD: u32 = 5;
pub const BIT_VIEW_AUDIT_LOG: u32 = 7;
pub const BIT_VIEW_CHANNEL: u32 = 10;
pub const BIT_SEND_MESSAGES: u32 = 11;
pub const BIT_MANAGE_MESSAGES: u32 = 13;
pub const BIT_READ_MESSAGE_HISTORY: u32 = 16;
pub const BIT_VIEW_GUILD_INSIGHTS: u32 = 19;
pub const BIT_CHANGE_NICKNAME: u32 = 26;
pub const BIT_MANAGE_NICKNAMES: u32 = 27;
pub const BIT_MANAGE_GUILD_EXPRESSIONS: u32 = 30;
pub const BIT_MANAGE_THREADS: u32 = 34;
pub const BIT_SEND_MESSAGES_IN_THREADS: u32 = 38;
pub const BIT_MODERATE_MEMBERS: u32 = 40;
pub const BIT_VIEW_CREATOR_MONETIZATION_ANALYTICS: u32 = 41;
pub const BIT_CREATE_GUILD_EXPRESSIONS: u32 = 43;

pub(crate) const GUILD_ONLY_PERMISSION_BITS: &[u32] = &[
    BIT_KICK_MEMBERS,
    BIT_BAN_MEMBERS,
    BIT_ADMINISTRATOR,
    BIT_MANAGE_GUILD,
    BIT_VIEW_AUDIT_LOG,
    BIT_VIEW_GUILD_INSIGHTS,
    BIT_CHANGE_NICKNAME,
    BIT_MANAGE_NICKNAMES,
    BIT_MANAGE_GUILD_EXPRESSIONS,
    BIT_MODERATE_MEMBERS,
    BIT_VIEW_CREATOR_MONETIZATION_ANALYTICS,
    BIT_CREATE_GUILD_EXPRESSIONS,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionSet {
    limbs: Vec<u64>,
}

impl crate::memory::HeapSize for PermissionSet {
    fn heap_size(&self) -> usize {
        self.limbs
            .capacity()
            .saturating_mul(std::mem::size_of::<u64>())
    }
}

impl PermissionSet {
    pub fn zero() -> Self {
        Self { limbs: Vec::new() }
    }

    pub fn from_bits(bits: &[u32]) -> Self {
        let mut set = Self::zero();
        for bit in bits {
            set.set(*bit);
        }
        set
    }

    pub fn parse_decimal(input: &str) -> Result<Self, &'static str> {
        if input.is_empty() || input.len() > 128 || !input.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err("permission bitset must be unsigned decimal");
        }
        let mut out = Self::zero();
        for byte in input.bytes() {
            out.mul_small(10);
            out.add_small(u64::from(byte - b'0'));
        }
        Ok(out)
    }

    pub fn has(&self, bit: u32) -> bool {
        let limb = (bit / 64) as usize;
        let offset = bit % 64;
        self.limbs
            .get(limb)
            .is_some_and(|value| value & (1_u64 << offset) != 0)
    }

    pub fn has_any(&self, bits: &[u32]) -> bool {
        bits.iter().any(|bit| self.has(*bit))
    }

    pub fn set(&mut self, bit: u32) {
        let limb = (bit / 64) as usize;
        let offset = bit % 64;
        if self.limbs.len() <= limb {
            self.limbs.resize(limb + 1, 0);
        }
        self.limbs[limb] |= 1_u64 << offset;
    }

    pub fn unset(&mut self, bit: u32) {
        let limb = (bit / 64) as usize;
        let offset = bit % 64;
        if let Some(value) = self.limbs.get_mut(limb) {
            *value &= !(1_u64 << offset);
            while matches!(self.limbs.last(), Some(0)) {
                self.limbs.pop();
            }
        }
    }

    pub fn union_with(&mut self, other: &Self) {
        if self.limbs.len() < other.limbs.len() {
            self.limbs.resize(other.limbs.len(), 0);
        }
        for (index, limb) in other.limbs.iter().enumerate() {
            self.limbs[index] |= limb;
        }
    }

    pub fn remove_all(&mut self, other: &Self) {
        for (index, limb) in other.limbs.iter().enumerate() {
            if let Some(current) = self.limbs.get_mut(index) {
                *current &= !limb;
            }
        }
        while matches!(self.limbs.last(), Some(0)) {
            self.limbs.pop();
        }
    }

    fn mul_small(&mut self, rhs: u64) {
        let mut carry: u128 = 0;
        for limb in &mut self.limbs {
            let value = u128::from(*limb) * u128::from(rhs) + carry;
            *limb = value as u64;
            carry = value >> 64;
        }
        if carry > 0 {
            self.limbs.push(carry as u64);
        }
    }

    fn add_small(&mut self, rhs: u64) {
        let mut carry = rhs;
        let mut index = 0;
        while carry > 0 {
            if self.limbs.len() <= index {
                self.limbs.push(0);
            }
            let (value, overflow) = self.limbs[index].overflowing_add(carry);
            self.limbs[index] = value;
            carry = if overflow { 1 } else { 0 };
            index += 1;
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionDecision {
    pub status: i32,
    pub can: bool,
    pub why: &'static str,
}

impl PermissionDecision {
    pub fn allow() -> Self {
        Self {
            status: D7OK,
            can: true,
            why: "OK",
        }
    }

    pub fn deny(why: &'static str) -> Self {
        Self {
            status: D7OK,
            can: false,
            why,
        }
    }

    pub fn forbidden(why: &'static str) -> Self {
        Self {
            status: D7FOR,
            can: false,
            why,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemberPermissions {
    pub permissions: PermissionSet,
    pub is_owner: bool,
    pub timed_out: bool,
    pub top_role_position: i64,
}

impl MemberPermissions {
    pub fn new(permissions: PermissionSet) -> Self {
        Self {
            permissions,
            is_owner: false,
            timed_out: false,
            top_role_position: 0,
        }
    }

    pub fn administrator(&self) -> bool {
        self.is_owner || self.permissions.has(BIT_ADMINISTRATOR)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionResolver {
    bot: MemberPermissions,
}

impl PermissionResolver {
    pub fn new(bot: MemberPermissions) -> Self {
        Self { bot }
    }

    pub fn is_admin(&self, member: &MemberPermissions) -> bool {
        member.administrator()
    }

    pub fn can_send(
        &self,
        in_thread: bool,
        private_thread_joined: bool,
        archived: bool,
        locked: bool,
    ) -> PermissionDecision {
        if self.bot.timed_out {
            return PermissionDecision::forbidden(WHY_BOT_TIMED_OUT);
        }
        if self.bot.administrator() {
            return PermissionDecision::allow();
        }
        if !self.bot.permissions.has(BIT_VIEW_CHANNEL) {
            return PermissionDecision::deny("MISSING_VIEW_CHANNEL");
        }
        if !in_thread && !self.bot.permissions.has(BIT_SEND_MESSAGES) {
            return PermissionDecision::deny("MISSING_SEND_MESSAGES");
        }
        if in_thread && !self.bot.permissions.has(BIT_SEND_MESSAGES_IN_THREADS) {
            return PermissionDecision::deny("MISSING_SEND_MESSAGES_IN_THREADS");
        }
        if in_thread && !private_thread_joined && !self.bot.permissions.has(BIT_MANAGE_THREADS) {
            return PermissionDecision::deny("THREAD_NOT_JOINED");
        }
        if archived {
            return PermissionDecision::deny("THREAD_ARCHIVED");
        }
        if locked {
            return PermissionDecision::deny("THREAD_LOCKED");
        }
        PermissionDecision::allow()
    }

    pub fn can_delete(&self, own_message: bool, known_deletable_type: bool) -> PermissionDecision {
        self.can_delete_message(own_message, known_deletable_type, false, true)
    }

    pub fn can_delete_message(
        &self,
        own_message: bool,
        known_deletable_type: bool,
        requires_manage_messages: bool,
        requires_read_message_history: bool,
    ) -> PermissionDecision {
        if self.bot.timed_out {
            return PermissionDecision::forbidden(WHY_BOT_TIMED_OUT);
        }
        if self.bot.administrator() {
            return if known_deletable_type {
                PermissionDecision::allow()
            } else {
                PermissionDecision::deny("MESSAGE_TYPE_NOT_DELETABLE")
            };
        }
        if !self.bot.permissions.has(BIT_VIEW_CHANNEL) {
            return PermissionDecision::deny("MISSING_VIEW_CHANNEL");
        }
        if requires_read_message_history && !self.bot.permissions.has(BIT_READ_MESSAGE_HISTORY) {
            return PermissionDecision::deny("MISSING_READ_MESSAGE_HISTORY_FOR_SNAPSHOT");
        }
        if !known_deletable_type {
            return PermissionDecision::deny("MESSAGE_TYPE_NOT_DELETABLE");
        }
        if (!own_message || requires_manage_messages)
            && !self.bot.permissions.has(BIT_MANAGE_MESSAGES)
        {
            return PermissionDecision::deny("MISSING_MANAGE_MESSAGES");
        }
        PermissionDecision::allow()
    }

    pub fn can_timeout(&self, target: &MemberPermissions) -> PermissionDecision {
        if self.bot.timed_out {
            return PermissionDecision::forbidden(WHY_BOT_TIMED_OUT);
        }
        if target.is_owner {
            return PermissionDecision::deny(WHY_TARGET_IS_OWNER);
        }
        if target.administrator() {
            return PermissionDecision::deny("TARGET_IS_ADMIN");
        }
        if !self.bot.administrator() && !self.bot.permissions.has(BIT_MODERATE_MEMBERS) {
            return PermissionDecision::deny("MISSING_MODERATE_MEMBERS");
        }
        if !self.bot.is_owner && self.bot.top_role_position <= target.top_role_position {
            return PermissionDecision::deny("ROLE_TOO_LOW_POLICY");
        }
        PermissionDecision::allow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn individual_permission_bits_can_be_unset_across_limb_boundaries() {
        let mut expected = vec![0, 3, 63, 64, 127, 128, 400];
        let mut permissions = PermissionSet::from_bits(&expected);
        for bit in [64, 0, 400, 128, 63, 127, 3] {
            permissions.unset(bit);
            expected.retain(|stored| *stored != bit);
            assert_eq!(permissions, PermissionSet::from_bits(&expected));
            assert!(!permissions.has(bit));
            permissions.unset(bit);
            assert_eq!(permissions, PermissionSet::from_bits(&expected));
        }
        assert_eq!(permissions, PermissionSet::zero());
    }

    #[test]
    fn unsetting_missing_high_permission_bits_does_not_allocate_or_change_permissions() {
        let mut permissions = PermissionSet::from_bits(&[3, 80]);
        let expected = permissions.clone();
        let capacity = permissions.limbs.capacity();
        for bit in [0, 79, 81, u32::MAX] {
            permissions.unset(bit);
            assert_eq!(permissions, expected);
            assert_eq!(permissions.limbs.capacity(), capacity);
        }
        let mut empty = PermissionSet::zero();
        empty.unset(u32::MAX);
        assert_eq!(empty, PermissionSet::zero());
        assert_eq!(empty.limbs.capacity(), 0);
    }

    fn bot(bits: &[u32]) -> PermissionResolver {
        PermissionResolver::new(MemberPermissions::new(PermissionSet::from_bits(bits)))
    }

    #[test]
    fn permission_set_parses_decimal_above_u64() {
        let set = PermissionSet::parse_decimal("1208925819614629174706176").unwrap();
        assert!(set.has(80));
        assert!(!set.has(79));
    }

    #[test]
    fn rejects_unbounded_permission_bitsets() {
        assert!(PermissionSet::parse_decimal(&"9".repeat(128)).is_ok());
        assert!(PermissionSet::parse_decimal(&"9".repeat(129)).is_err());
    }

    #[test]
    fn thread_send_uses_thread_permission_instead_of_parent_send_permission() {
        let resolver = bot(&[BIT_VIEW_CHANNEL, BIT_SEND_MESSAGES_IN_THREADS]);
        assert_eq!(
            resolver.can_send(true, true, false, false),
            PermissionDecision::allow()
        );
        assert_eq!(
            resolver.can_send(false, true, false, false),
            PermissionDecision::deny("MISSING_SEND_MESSAGES")
        );
        assert!(!resolver.can_send(true, false, false, false).can);
        assert!(!resolver.can_send(true, true, true, false).can);
    }

    #[test]
    fn administrator_overrides_send_checks() {
        let resolver = bot(&[BIT_ADMINISTRATOR]);
        assert_eq!(
            resolver.can_send(true, false, true, true),
            PermissionDecision::allow()
        );
    }

    #[test]
    fn manage_threads_allows_private_access_but_still_needs_view_and_send_permissions() {
        let resolver = bot(&[
            BIT_VIEW_CHANNEL,
            BIT_SEND_MESSAGES_IN_THREADS,
            BIT_MANAGE_THREADS,
        ]);
        assert!(resolver.can_send(true, false, false, false).can);
        assert_eq!(
            bot(&[BIT_MANAGE_THREADS])
                .can_send(true, false, false, false)
                .why,
            "MISSING_VIEW_CHANNEL"
        );
        assert_eq!(
            bot(&[BIT_MANAGE_THREADS, BIT_VIEW_CHANNEL])
                .can_send(true, false, false, false)
                .why,
            "MISSING_SEND_MESSAGES_IN_THREADS"
        );
    }

    #[test]
    fn send_reports_specific_missing_permission() {
        let resolver = bot(&[BIT_VIEW_CHANNEL, BIT_SEND_MESSAGES]);
        assert_eq!(
            resolver.can_send(true, true, false, false),
            PermissionDecision::deny("MISSING_SEND_MESSAGES_IN_THREADS")
        );
    }

    #[test]
    fn delete_other_message_requires_manage_messages() {
        let resolver = bot(&[BIT_VIEW_CHANNEL, BIT_READ_MESSAGE_HISTORY]);
        assert_eq!(
            resolver.can_delete(false, true),
            PermissionDecision::deny("MISSING_MANAGE_MESSAGES")
        );
    }

    #[test]
    fn delete_auto_moderation_action_requires_manage_messages_even_when_own() {
        let resolver = bot(&[BIT_VIEW_CHANNEL, BIT_READ_MESSAGE_HISTORY]);
        assert_eq!(
            resolver.can_delete_message(true, true, true, true),
            PermissionDecision::deny("MISSING_MANAGE_MESSAGES")
        );
        let resolver = bot(&[
            BIT_VIEW_CHANNEL,
            BIT_READ_MESSAGE_HISTORY,
            BIT_MANAGE_MESSAGES,
        ]);
        assert_eq!(
            resolver.can_delete_message(true, true, true, true),
            PermissionDecision::allow()
        );
    }

    #[test]
    fn delete_can_skip_read_message_history_for_complete_snapshot() {
        let resolver = bot(&[BIT_VIEW_CHANNEL]);
        assert_eq!(
            resolver.can_delete_message(true, true, false, false),
            PermissionDecision::allow()
        );
        assert_eq!(
            resolver.can_delete_message(true, true, false, true),
            PermissionDecision::deny("MISSING_READ_MESSAGE_HISTORY_FOR_SNAPSHOT")
        );
    }

    #[test]
    fn timeout_checks_hierarchy() {
        let mut bot_member =
            MemberPermissions::new(PermissionSet::from_bits(&[BIT_MODERATE_MEMBERS]));
        bot_member.top_role_position = 10;
        let resolver = PermissionResolver::new(bot_member);
        let mut target = MemberPermissions::new(PermissionSet::zero());
        target.top_role_position = 10;
        assert_eq!(
            resolver.can_timeout(&target),
            PermissionDecision::deny("ROLE_TOO_LOW_POLICY")
        );
        target.top_role_position = 9;
        assert_eq!(resolver.can_timeout(&target), PermissionDecision::allow());
    }

    #[test]
    fn administrator_timeout_still_respects_the_target_role_hierarchy() {
        let mut bot = MemberPermissions::new(PermissionSet::from_bits(&[BIT_ADMINISTRATOR]));
        bot.top_role_position = 10;
        let resolver = PermissionResolver::new(bot);
        let mut target = MemberPermissions::new(PermissionSet::zero());
        for position in [10, 11] {
            target.top_role_position = position;
            assert_eq!(
                resolver.can_timeout(&target),
                PermissionDecision::deny("ROLE_TOO_LOW_POLICY")
            );
        }
        target.top_role_position = 9;
        assert_eq!(resolver.can_timeout(&target), PermissionDecision::allow());
    }

    #[test]
    fn guild_owner_permissions_keep_the_owner_exception_and_protected_targets() {
        let mut owner = MemberPermissions::new(PermissionSet::zero());
        owner.is_owner = true;
        let resolver = PermissionResolver::new(owner);
        let mut target = MemberPermissions::new(PermissionSet::zero());
        target.top_role_position = 100;
        assert_eq!(resolver.can_timeout(&target), PermissionDecision::allow());
        target.is_owner = true;
        assert!(!resolver.can_timeout(&target).can);
        target.is_owner = false;
        target.permissions.set(BIT_ADMINISTRATOR);
        assert!(!resolver.can_timeout(&target).can);
    }

    #[test]
    fn administrator_cannot_delete_an_undeletable_message_type() {
        let resolver = bot(&[BIT_ADMINISTRATOR]);
        assert_eq!(
            resolver.can_delete(true, false),
            PermissionDecision::deny("MESSAGE_TYPE_NOT_DELETABLE")
        );
        assert_eq!(
            resolver.can_delete(false, true),
            PermissionDecision::allow()
        );
    }
}
