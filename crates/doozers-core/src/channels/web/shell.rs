//! The frame around every signed-in page (`templates/web/app.html`): the sidebar's navigation, with
//! the page's own link marked current, and the signed-in user with the logout button. The design is
//! in `Docs/technical/web-ui.md`.

use super::admin::may_administer;
use super::auth::WebUser;

/// The sidebar link a page belongs to: its own, or its parent's for a page under it (the job form
/// belongs to [`Nav::Jobs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Nav {
    Chats,
    Jobs,
    Connections,
    SshKeys,
    Account,
    Status,
    Plugins,
    Agents,
    Users,
    Routes,
    McpServers,
    Workspaces,
    AdminSshKeys,
    AdminSshHosts,
    AdminIdentityProviders,
}

/// The links of the "You" group, then of the "Admin" group: (page, label, href).
const YOU: [(Nav, &str, &str); 5] = [
    (Nav::Chats, "Chats", "/"),
    (Nav::Jobs, "Scheduled jobs", "/jobs"),
    (Nav::Connections, "Connections", "/connections"),
    (Nav::SshKeys, "SSH keys", "/ssh-keys"),
    (Nav::Account, "Account", "/account"),
];
const ADMIN: [(Nav, &str, &str); 10] = [
    (Nav::Status, "Status", "/admin"),
    (Nav::Plugins, "Plugins", "/admin/plugins"),
    (Nav::Agents, "Agents", "/admin/agents"),
    (Nav::Users, "Users", "/admin/users"),
    (Nav::Routes, "Routes", "/admin/routes"),
    (Nav::McpServers, "MCP servers", "/admin/mcp-servers"),
    (Nav::Workspaces, "Workspaces", "/admin/workspaces"),
    (Nav::AdminSshKeys, "SSH keys", "/admin/ssh-keys"),
    (Nav::AdminSshHosts, "Known SSH hosts", "/admin/ssh-hosts"),
    (
        Nav::AdminIdentityProviders,
        "Identity providers",
        "/admin/identity-providers",
    ),
];

/// What `app.html` needs: every page that extends it has a `shell` field.
pub(super) struct Shell {
    /// The signed-in user's display name, shown in the sidebar's footer.
    pub(super) name: String,
    /// The first letter of [`Self::name`], capitalized: the footer's badge.
    pub(super) initial: String,
    /// The navigation groups, the admin one only for a user who may administer.
    pub(super) groups: Vec<NavGroup>,
    /// The current link's label: the narrow screens' top bar shows it.
    pub(super) section: &'static str,
    /// Set for a cookie login: the footer has a logout button posting this token.
    pub(super) logout_csrf: Option<String>,
}

pub(super) struct NavGroup {
    pub(super) label: &'static str,
    pub(super) links: Vec<NavLink>,
}

pub(super) struct NavLink {
    pub(super) label: &'static str,
    pub(super) href: &'static str,
    pub(super) current: bool,
}

impl Shell {
    /// The frame of a page that belongs to `current`, for `user`.
    pub(super) fn new(user: &WebUser, current: Nav) -> Self {
        let group = |label, links: &[(Nav, &'static str, &'static str)]| NavGroup {
            label,
            links: links
                .iter()
                .map(|&(nav, label, href)| NavLink {
                    label,
                    href,
                    current: nav == current,
                })
                .collect(),
        };
        let mut groups = vec![group("You", &YOU)];
        if may_administer(user) {
            groups.push(group("Admin", &ADMIN));
        }
        let section = YOU
            .iter()
            .chain(&ADMIN)
            .find(|(nav, _, _)| *nav == current)
            .map_or("Doozers", |(_, label, _)| label);
        let name = user.display_name.as_str();
        Self {
            name: name.to_owned(),
            initial: name
                .chars()
                .next()
                .map(char::to_uppercase)
                .map_or_else(String::new, String::from_iter),
            groups,
            section,
            logout_csrf: user.can_log_out().then(|| user.csrf.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use doozers_types::{NativeId, UserId};

    use super::*;
    use crate::channels::web::auth::Login;

    fn user(id: &str) -> WebUser {
        WebUser {
            user: UserId::new(id).unwrap(),
            role: if id == "owner" {
                doozers_store::Role::Admin
            } else {
                doozers_store::Role::Member
            },
            sender: NativeId::new(id).unwrap(),
            csrf: "token".into(),
            display_name: format!("{id} (display)"),
            login: Login::Test,
        }
    }

    fn labels(shell: &Shell) -> Vec<(&str, Vec<&str>)> {
        shell
            .groups
            .iter()
            .map(|g| (g.label, g.links.iter().map(|l| l.label).collect()))
            .collect()
    }

    #[test]
    fn the_owner_sees_the_admin_group_with_the_page_marked_current() {
        let shell = Shell::new(&user("owner"), Nav::Agents);
        assert_eq!(
            labels(&shell),
            [
                (
                    "You",
                    vec![
                        "Chats",
                        "Scheduled jobs",
                        "Connections",
                        "SSH keys",
                        "Account"
                    ]
                ),
                (
                    "Admin",
                    vec![
                        "Status",
                        "Plugins",
                        "Agents",
                        "Users",
                        "Routes",
                        "MCP servers",
                        "Workspaces",
                        "SSH keys",
                        "Known SSH hosts",
                        "Identity providers"
                    ]
                ),
            ]
        );
        let current: Vec<_> = shell
            .groups
            .iter()
            .flat_map(|g| &g.links)
            .filter(|l| l.current)
            .map(|l| l.href)
            .collect();
        assert_eq!(current, ["/admin/agents"]);
        assert_eq!(shell.section, "Agents");
        assert_eq!(
            (shell.name.as_str(), shell.initial.as_str()),
            ("owner (display)", "O")
        );
    }

    #[test]
    fn anyone_else_sees_only_their_own_pages() {
        let shell = Shell::new(&user("guest"), Nav::Jobs);
        assert_eq!(
            labels(&shell),
            [(
                "You",
                vec![
                    "Chats",
                    "Scheduled jobs",
                    "Connections",
                    "SSH keys",
                    "Account"
                ]
            )]
        );
        assert_eq!(
            (shell.name.as_str(), shell.initial.as_str()),
            ("guest (display)", "G")
        );
    }

    #[test]
    fn the_footer_shows_the_display_name_not_the_id() {
        let mut ann = user("u-4f9c");
        ann.display_name = "ann example".to_owned();
        let shell = Shell::new(&ann, Nav::Account);
        assert_eq!(
            (shell.name.as_str(), shell.initial.as_str()),
            ("ann example", "A")
        );
        assert_eq!(shell.section, "Account");
    }
}
