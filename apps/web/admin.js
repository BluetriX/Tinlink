// Group settings: overview, roles, members, channels with permission
// overwrites, and invites. Shown in place of the chat; the server enforces
// every rule, the UI only hides or disables what would be refused anyway.
import {
  CHANNEL_PERMISSIONS, PERMISSIONS, can, canEditRole, formatPerms, memberPosition, overwriteState, parsePerms,
  roleColor, setOverwriteState,
} from "./logic.js";

const TABS = [
  ["overview", "Overview", (p) => can(p, "MANAGE_GROUP")],
  ["roles", "Roles", (p) => can(p, "MANAGE_ROLES")],
  ["members", "Members", (p) => can(p, "MANAGE_ROLES") || can(p, "KICK_MEMBERS")],
  ["channels", "Channels", (p) => can(p, "MANAGE_CHANNELS")],
  ["invites", "Invites", (p) => can(p, "MANAGE_GROUP")],
];

/** Whether the group permissions allow any settings tab. */
export function hasSettings(perms) {
  return TABS.some(([, , allowed]) => allowed(perms));
}

/**
 * ctx: { api, el, ask, tryText, setStatus, state, onClose, onGroupDeleted }
 * where `state` is the app state (group, members, categories, channels, session).
 */
export function createSettings(ctx) {
  const { api, el, ask, tryText, setStatus, state } = ctx;
  // `dirty`: the user changed a field that is not saved yet; live updates
  // then fetch data but do not redraw over the edit.
  const view = { open: false, tab: null, roles: [], invites: [], editing: null, dirty: false };
  const $ = (id) => document.getElementById(id);
  for (const type of ["input", "change"]) {
    $("settings-body").addEventListener(type, () => {
      view.dirty = true;
    });
  }

  const group = () => state.group;
  const perms = () => state.group?.permissions;
  const me = () => state.session?.user.id;
  const myPosition = () => state.group?.my_position ?? 0;
  const isOwner = () => state.group?.owner_id === me();

  /** Runs an API call; on failure shows the server's message and returns undefined. */
  async function call(fn, done) {
    try {
      const result = await fn();
      if (done) setStatus(done);
      return result ?? true;
    } catch (e) {
      setStatus(e.message || String(e), true);
      return undefined;
    }
  }

  function memberName(userId) {
    const m = state.members.get(userId);
    return m ? m.display_name || m.name : "unknown";
  }

  async function open(tab) {
    const tabs = TABS.filter(([, , allowed]) => allowed(perms()));
    if (!tabs.length) return;
    view.open = true;
    view.tab = tabs.some(([id]) => id === tab) ? tab : view.tab && tabs.some(([id]) => id === view.tab) ? view.tab : tabs[0][0];
    view.editing = null;
    $("settings").hidden = false;
    await reload();
  }

  function close() {
    if (!view.open) return;
    view.open = false;
    $("settings").hidden = true;
  }

  /** Reloads the data of the open tab and redraws it. */
  async function reload({ keepEdits = false } = {}) {
    if (!view.open || !group()) return;
    if (!TABS.find(([id]) => id === view.tab)?.[2](perms())) {
      // Lost the permission for this tab.
      const next = TABS.find(([, , allowed]) => allowed(perms()));
      if (!next) {
        close();
        ctx.onClose();
        return;
      }
      view.tab = next[0];
    }
    const groupId = group().id;
    if (["roles", "members", "channels"].includes(view.tab)) {
      const roles = await call(() => api("GET", `/groups/${groupId}/roles`));
      if (Array.isArray(roles)) view.roles = roles;
    }
    if (view.tab === "invites") {
      const invites = await call(() => api("GET", `/groups/${groupId}/invites`));
      if (Array.isArray(invites)) view.invites = invites;
    }
    if (!view.open || group()?.id !== groupId) return;
    if (keepEdits && view.dirty) return;
    render();
  }

  function render() {
    view.dirty = false;
    const tabs = TABS.filter(([, , allowed]) => allowed(perms()));
    $("settings-tabs").replaceChildren(
      ...tabs.map(([id, label]) => el("button", {
        type: "button",
        class: `small${id === view.tab ? " active" : ""}`,
        textContent: label,
        dataset: { tab: id },
        onclick: () => {
          view.tab = id;
          view.editing = null;
          reload();
        },
      })),
      el("span", { class: "spacer" }),
      el("button", {
        type: "button", class: "small", textContent: "Close", id: "settings-close",
        onclick: () => {
          close();
          ctx.onClose();
        },
      }));
    const body = { overview, roles, members, channels, invites }[view.tab]();
    $("settings-body").replaceChildren(...[body].flat());
  }

  const field = (label, input) => el("label", { class: "field" }, el("span", { textContent: label }), input);
  const section = (title, ...children) => el("section", { class: "settings-section" }, el("h3", { textContent: title }), ...children);

  // ------------------------------------------------------------ overview

  function overview() {
    const g = group();
    const name = el("input", { value: g.name, maxLength: 64 });
    const limit = el("input", { type: "number", min: 0, max: 50, value: String(g.user_channel_limit) });
    const parts = [section("Group",
      field("Name", name),
      field("Own channels per member", limit),
      el("p", { class: "hint", textContent: "How many channels a member may create in categories that allow it." }),
      el("button", {
        type: "button", textContent: "Save",
        onclick: () => call(() => api("PATCH", `/groups/${g.id}`, {
          name: name.value.trim(),
          user_channel_limit: Number(limit.value),
        }), "Group saved"),
      }))];
    if (isOwner()) {
      parts.push(section("Danger zone",
        el("p", { class: "hint", textContent: "Deleting the group removes all channels, messages and files for everyone." }),
        el("button", {
          type: "button", class: "danger", textContent: "Delete group…",
          onclick: async () => {
            const ok = await ask({
              title: `Delete ${g.name}?`,
              text: "Type the group's name to confirm. This cannot be undone.",
              fields: [{ id: "name", label: "Group name" }],
              ok: "Delete",
              submit: (v) => v.name !== g.name ? "The name does not match." : tryText(() => api("DELETE", `/groups/${g.id}`)),
            });
            if (ok) {
              close();
              ctx.onGroupDeleted();
            }
          },
        })));
    }
    return parts;
  }

  // --------------------------------------------------------------- roles

  function roles() {
    const groupId = group().id;
    const add = el("button", {
      type: "button", textContent: "New role",
      onclick: () => ask({
        title: "New role",
        fields: [{ id: "name", label: "Name" }],
        submit: (v) => v.name ? tryText(async () => {
          const role = await api("POST", `/groups/${groupId}/roles`, { name: v.name, position: 1 });
          view.editing = role.id;
          await reload();
        }) : "Enter a name.",
      }),
    });
    const list = view.roles.map((r) => roleCard(r));
    return [
      section("Roles",
        el("p", { class: "hint", textContent: "Higher roles come first. You can only change roles below your own highest role and only grant permissions you have." }),
        add,
        ...list),
    ];
  }

  function roleCard(role) {
    const editable = canEditRole(role, myPosition());
    const expanded = view.editing === role.id;
    const head = el("button", {
      type: "button", class: "role-head",
      onclick: () => {
        view.editing = expanded ? null : role.id;
        render();
      },
    },
    el("span", { class: "swatch", style: `background: ${roleColor(role.color) || "var(--border)"}` }),
    el("strong", { textContent: role.everyone ? "@everyone" : role.name }),
    el("span", { class: "muted", textContent: role.everyone ? " all members" : ` position ${role.position}` }),
    editable ? null : el("span", { class: "muted", textContent: " (read only)" }));
    if (!expanded) return el("div", { class: "role", dataset: { role: role.name } }, head);

    const name = el("input", { value: role.name, maxLength: 64, disabled: !editable || role.everyone });
    const color = el("input", { type: "color", value: roleColor(role.color) || "#808080", disabled: !editable || role.everyone });
    const position = el("input", {
      type: "number", min: 1, max: Math.max(1, Math.min(myPosition() - 1, 1000)), value: String(role.position),
      disabled: !editable || role.everyone,
    });
    const flags = parsePerms(role.permissions);
    const boxes = PERMISSIONS.map(([flag, label]) => {
      const box = el("input", {
        type: "checkbox", checked: flags.has(flag), disabled: !editable || !can(perms(), flag),
        dataset: { flag },
      });
      return el("label", { class: "inline perm" }, box, ` ${label}`);
    });
    const save = el("button", {
      type: "button", textContent: "Save", disabled: !editable,
      onclick: async () => {
        const chosen = new Set();
        for (const label of boxes) {
          const box = label.querySelector("input");
          if (box.checked) chosen.add(box.dataset.flag);
        }
        const body = { permissions: formatPerms(chosen) };
        if (!role.everyone) {
          body.name = name.value.trim();
          body.color = parseInt(color.value.slice(1), 16);
          body.position = Number(position.value);
        }
        if (await call(() => api("PATCH", `/groups/${group().id}/roles/${role.id}`, body), "Role saved")) reload();
      },
    });
    const remove = role.everyone ? null : el("button", {
      type: "button", class: "danger", textContent: "Delete role", disabled: !editable,
      onclick: async () => {
        const ok = await ask({ title: `Delete ${role.name}?`, text: "Members lose this role and its permissions.", ok: "Delete" });
        if (ok && await call(() => api("DELETE", `/groups/${group().id}/roles/${role.id}`), "Role deleted")) {
          view.editing = null;
          reload();
        }
      },
    });
    return el("div", { class: "role open", dataset: { role: role.name } },
      head,
      role.everyone ? null : el("div", { class: "row" }, field("Name", name), field("Color", color), field("Position", position)),
      el("div", { class: "perm-grid" }, ...boxes),
      el("div", { class: "row" }, save, remove));
  }

  // ------------------------------------------------------------- members

  function members() {
    const groupId = group().id;
    const assignable = view.roles.filter((r) => !r.everyone && r.position < myPosition() && can(perms(), "MANAGE_ROLES"));
    const others = view.roles.filter((r) => !r.everyone);
    const list = [...state.members.values()].sort((a, b) => (a.display_name || a.name).localeCompare(b.display_name || b.name));
    return section("Members",
      el("p", { class: "hint", textContent: "Tick roles to assign them. You can only manage members below your own highest role." }),
      ...list.map((m) => {
        const owner = m.user_id === group().owner_id;
        const below = !owner && memberPosition(m, view.roles) < myPosition();
        const chips = others.map((r) => {
          const box = el("input", {
            type: "checkbox", checked: (m.role_ids || []).includes(r.id),
            disabled: !assignable.includes(r),
            onchange: async () => {
              const method = box.checked ? "PUT" : "DELETE";
              const ok = await call(() => api(method, `/groups/${groupId}/members/${m.user_id}/roles/${r.id}`),
                box.checked ? `${r.name} assigned` : `${r.name} removed`);
              if (!ok) box.checked = !box.checked;
            },
          });
          return el("label", { class: "inline chip" }, box, ` ${r.name}`);
        });
        const kick = can(perms(), "KICK_MEMBERS") && m.user_id !== me() && below ? el("button", {
          type: "button", class: "small danger", textContent: "Kick",
          onclick: async () => {
            const ok = await ask({ title: `Kick ${m.display_name || m.name}?`, text: "They can rejoin with a new invite.", ok: "Kick" });
            if (ok) await call(() => api("DELETE", `/groups/${groupId}/members/${m.user_id}`), "Member removed");
          },
        }) : null;
        return el("div", { class: "member-row", dataset: { user: m.name } },
          el("span", { class: `dot${m.online ? " online" : ""}` }),
          el("strong", { textContent: m.display_name || m.name }),
          owner ? el("span", { class: "muted", textContent: " owner" }) : null,
          el("span", { class: "chips" }, ...chips),
          kick);
      }));
  }

  // ------------------------------------------------------------ channels

  function channels() {
    if (view.editing?.startsWith?.("ow:")) return overwriteEditor(view.editing.slice(3));
    const groupId = group().id;
    const cats = [...state.categories].sort((a, b) => a.position - b.position);
    const catRows = cats.map((c) => {
      const name = el("input", { value: c.name, maxLength: 64 });
      const position = el("input", { type: "number", value: String(c.position), class: "narrow" });
      const allow = el("input", { type: "checkbox", checked: c.allow_user_channels });
      return el("div", { class: "row item", dataset: { category: c.name } },
        name, position,
        el("label", { class: "inline", title: "Members with “Create own channels” may create channels here" }, allow, " member channels"),
        el("button", {
          type: "button", class: "small", textContent: "Save",
          onclick: () => call(() => api("PATCH", `/categories/${c.id}`, {
            name: name.value.trim(), position: Number(position.value), allow_user_channels: allow.checked,
          }), "Category saved"),
        }),
        el("button", {
          type: "button", class: "small danger", textContent: "Delete",
          onclick: async () => {
            const ok = await ask({ title: `Delete category ${c.name}?`, text: "Its channels stay, without a category.", ok: "Delete" });
            if (ok) await call(() => api("DELETE", `/categories/${c.id}`), "Category deleted");
          },
        }));
    });
    const chans = [...state.channels].sort((a, b) => a.position - b.position || a.name.localeCompare(b.name));
    const chanRows = chans.map((ch) => {
      const name = el("input", { value: ch.name, maxLength: 64 });
      const position = el("input", { type: "number", value: String(ch.position), class: "narrow" });
      const voiceBox = el("input", { type: "checkbox", checked: ch.voice_enabled });
      const category = el("select", {},
        el("option", { value: "", textContent: "(no category)" }),
        ...cats.map((c) => el("option", { value: c.id, textContent: c.name })));
      category.value = ch.category_id || "";
      return el("div", { class: "row item", dataset: { channel: ch.name } },
        name, position,
        el("label", { class: "inline" }, voiceBox, " voice"),
        category,
        el("button", {
          type: "button", class: "small", textContent: "Save",
          onclick: () => call(() => api("PATCH", `/channels/${ch.id}`, {
            name: name.value.trim(), position: Number(position.value), voice_enabled: voiceBox.checked,
            category_id: category.value || null,
          }), "Channel saved"),
        }),
        el("button", {
          type: "button", class: "small", textContent: "Permissions",
          onclick: () => {
            view.editing = `ow:${ch.id}`;
            loadOverwrites(ch.id);
          },
        }),
        el("button", {
          type: "button", class: "small danger", textContent: "Delete",
          onclick: async () => {
            const ok = await ask({ title: `Delete #${ch.name}?`, text: "All its messages and files are deleted.", ok: "Delete" });
            if (ok) await call(() => api("DELETE", `/channels/${ch.id}`), "Channel deleted");
          },
        }));
    });
    return [
      section("Categories",
        el("button", {
          type: "button", textContent: "New category",
          onclick: () => ask({
            title: "New category",
            fields: [{ id: "name", label: "Name" }, { id: "allow", label: "Members may create channels here", type: "checkbox" }],
            submit: (v) => v.name ? tryText(() => api("POST", `/groups/${groupId}/categories`, {
              name: v.name, allow_user_channels: v.allow, position: cats.length ? Math.max(...cats.map((c) => c.position)) + 1 : 0,
            })) : "Enter a name.",
          }),
        }),
        ...catRows),
      section("Channels",
        el("p", { class: "hint", textContent: "New channels are created with + in the channel list. Lower positions come first." }),
        ...chanRows),
    ];
  }

  const overwrites = { channelId: null, list: [] };

  async function loadOverwrites(channelId) {
    const list = await call(() => api("GET", `/channels/${channelId}/overwrites`));
    if (!Array.isArray(list)) return;
    overwrites.channelId = channelId;
    overwrites.list = list;
    render();
  }

  function targetName(t) {
    if (t.kind === "role") {
      const r = view.roles.find((x) => x.id === t.id);
      return r ? (r.everyone ? "@everyone" : `@${r.name}`) : "deleted role";
    }
    return memberName(t.id);
  }

  function overwriteEditor(channelId) {
    const ch = state.channels.find((c) => c.id === channelId);
    if (!ch || overwrites.channelId !== channelId) {
      if (ch) loadOverwrites(channelId);
      return el("p", { class: "muted", textContent: "Loading…" });
    }
    const back = el("button", {
      type: "button", class: "small", textContent: "← Channels",
      onclick: () => {
        view.editing = null;
        render();
      },
    });
    const present = new Set(overwrites.list.map((o) => `${o.target.kind}:${o.target.id}`));
    const choices = [
      ...view.roles.map((r) => [`role:${r.id}`, r.everyone ? "@everyone" : `@${r.name}`]),
      ...[...state.members.values()].map((m) => [`member:${m.user_id}`, m.display_name || m.name]),
    ].filter(([key]) => !present.has(key));
    const picker = el("select", {}, ...choices.map(([value, label]) => el("option", { value, textContent: label })));
    const add = el("button", {
      type: "button", class: "small", textContent: "Add", disabled: !choices.length,
      onclick: () => {
        const [kind, id] = picker.value.split(":");
        overwrites.list.push({ target: { kind, id }, allow: "", deny: "", unsaved: true });
        render();
      },
    });
    const cards = overwrites.list.map((ow, index) => {
      const rows = CHANNEL_PERMISSIONS.map(([flag, label]) => {
        const select = el("select", {
          dataset: { flag },
          disabled: !can(ch.permissions, flag),
          onchange: () => {
            overwrites.list[index] = setOverwriteState(overwrites.list[index], flag, select.value);
          },
        },
        el("option", { value: "inherit", textContent: "inherit" }),
        el("option", { value: "allow", textContent: "allow" }),
        el("option", { value: "deny", textContent: "deny" }));
        select.value = overwriteState(ow, flag);
        return el("label", { class: "ow-row" }, el("span", { textContent: label }), select);
      });
      const path = `/channels/${channelId}/overwrites/${ow.target.kind}/${ow.target.id}`;
      return el("div", { class: "overwrite", dataset: { target: targetName(ow.target) } },
        el("strong", { textContent: targetName(ow.target) + (ow.unsaved ? " (not saved)" : "") }),
        el("div", { class: "ow-grid" }, ...rows),
        el("div", { class: "row" },
          el("button", {
            type: "button", class: "small", textContent: "Save",
            onclick: async () => {
              const current = overwrites.list[index];
              if (await call(() => api("PUT", path, { allow: current.allow, deny: current.deny }), "Permissions saved")) {
                loadOverwrites(channelId);
              }
            },
          }),
          el("button", {
            type: "button", class: "small danger", textContent: "Remove",
            onclick: async () => {
              if (ow.unsaved || await call(() => api("DELETE", path), "Overwrite removed")) {
                overwrites.list.splice(index, 1);
                if (ow.unsaved) render();
                else loadOverwrites(channelId);
              }
            },
          })));
    });
    return section(`Permissions for #${ch.name}`,
      back,
      el("p", { class: "hint", textContent: "Overwrites change the role permissions in this channel: “deny” takes a permission away, “allow” grants it. Member entries win over role entries." }),
      el("div", { class: "row" }, picker, add),
      ...cards);
  }

  // ------------------------------------------------------------- invites

  function invites() {
    const groupId = group().id;
    const maxUses = el("input", { type: "number", min: 0, placeholder: "unlimited", class: "narrow" });
    const hours = el("input", { type: "number", min: 0, placeholder: "never", class: "narrow" });
    const rows = view.invites.map((inv) => el("div", { class: "row item", dataset: { code: inv.code } },
      el("code", { textContent: inv.code }),
      el("span", { class: "muted", textContent: `used ${inv.uses}${inv.max_uses ? ` of ${inv.max_uses}` : ""}` }),
      el("span", { class: "muted", textContent: inv.expires_at ? `expires ${new Date(inv.expires_at).toLocaleString()}` : "never expires" }),
      el("button", {
        type: "button", class: "small", textContent: "Copy",
        onclick: () => navigator.clipboard?.writeText(inv.code).then(() => setStatus("Code copied"), () => {}),
      }),
      el("button", {
        type: "button", class: "small danger", textContent: "Revoke",
        onclick: async () => {
          if (await call(() => api("DELETE", `/groups/${groupId}/invites/${encodeURIComponent(inv.code)}`), "Invite revoked")) reload();
        },
      })));
    return [
      section("New invite",
        el("div", { class: "row" },
          field("Max uses", maxUses),
          field("Expires after hours", hours),
          el("button", {
            type: "button", textContent: "Create",
            onclick: async () => {
              const inv = await call(() => api("POST", `/groups/${groupId}/invites`, {
                max_uses: Number(maxUses.value) || null,
                expires_in_hours: Number(hours.value) || null,
              }), "Invite created");
              if (inv) reload();
            },
          })),
        el("p", { class: "hint", textContent: `Others join with the code under + on the left, at ${location.origin}.` })),
      section("Open invites", ...(rows.length ? rows : [el("p", { class: "muted", textContent: "No open invites." })])),
    ];
  }

  return {
    open,
    close,
    /** After live updates (group_updated etc.); keeps unsaved edits on screen. */
    refresh: () => reload({ keepEdits: true }),
    get isOpen() {
      return view.open;
    },
  };
}
