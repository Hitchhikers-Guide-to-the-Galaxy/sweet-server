//! One Click Orgs, rewilded — the native mount of `oneclick-core` (Rewilding OneClick Orgs
//! Plan, Phase 3). An organisation IS a site: its constitution, permissions, register,
//! proposals and decisions are pages of typed items (see oneclick_core::pages), the journal
//! is the clause history, and every mutation is a command that folds the pages, runs the
//! domain, and writes the item-level diff back through the ONE write path, `actions::apply`,
//! so the wiki's journal carries it like any edit.
//!
//!   GET  /plugin/oneclick/constitution.html      the association constitution, byte-comparable (D0)
//!   GET  /plugin/oneclick/constitution.json      {name, kind, state, updated_at, html, clauses}
//!   GET  /plugin/oneclick/clauses.json           {name: {value_type, value, started_at}}
//!   GET  /plugin/oneclick/members.json           the register, with what the actor may do
//!   GET  /plugin/oneclick/proposals.json         every proposal with its tally
//!   GET  /plugin/oneclick/proposal/<slug>.json   one proposal, its votes and its system
//!   GET  /plugin/oneclick/decisions.json
//!   GET  /plugin/oneclick/fca.json               a co-op's registration-form rule numbers (D2)
//!   GET  /plugin/oneclick/voting-systems.json    the B0 table, from the candidate's own rules
//!   GET  /plugin/oneclick/lifecycles.json        the B1-B3 table
//!   POST /plugin/oneclick/command                {command, actor, ...} — owner-authorised
//!        commands: propose, found, vote, add-founding-member, member-event, set-class, admit,
//!        coop-propose, coop-found, add-director, stand-down, edit-profile, set-articles-url,
//!        adopt-audit-committee, set-group, appoint-representative, audit (approve|block)
//!   POST /plugin/oneclick/close                  close due and early-closable proposals (the
//!                                                reference's 60-second job, run on demand)
//!
//! Identity this weekend: the acting member is the `actor` field, trusted on an
//! owner-authorised request (friends mode has one owner per site). Per-member sign-in is
//! the plan's named slip.

use crate::{actions, load_page, not_found, Site};
use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::Response;
use oneclick_core::members::MemberEvent;
use oneclick_core::org::{OrgState, Organisation};
use oneclick_core::pages;
use oneclick_core::proposals::{ProposalKind, ProposalState, Role};
use oneclick_core::render::association::{render, AssociationConstitution};
use serde_json::{json, Value};

const PREFIX: &str = "/plugin/oneclick/";

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn json(status: StatusCode, v: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(v.to_string()))
        .unwrap()
}

fn html(s: String) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(s))
        .unwrap()
}

fn page_value(site: &Site, slug: &str) -> Option<Value> {
    load_page(site, slug).and_then(|b| serde_json::from_slice(&b).ok())
}

/// Every page that carries a `proposal` item — a proposal's page is its slug.
fn proposal_slugs(site: &Site) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(site.data.join("pages")) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if let Some(p) = page_value(site, &name) {
                if p["story"].as_array().map(|s| s.iter().any(|i| i["type"] == "proposal")).unwrap_or(false) {
                    out.push(name);
                }
            }
        }
    }
    out.sort();
    out
}

/// Fold the organisation from the site's pages; None when this site is not an organisation.
fn fold(site: &Site) -> Option<(Organisation, String)> {
    let slugs = proposal_slugs(site);
    let lookup = |slug: &str| -> Option<Value> {
        if slug == "__proposal_slugs__" { return Some(json!(slugs)); }
        page_value(site, slug)
    };
    let org = pages::fold(&lookup)?;
    let domain = page_value(site, pages::ORG_SLUG)
        .and_then(|p| p["story"].as_array().cloned())
        .and_then(|s| s.iter().find(|i| i["type"] == "oneclick-org").and_then(|i| i["domain"].as_str().map(String::from)))
        .unwrap_or_default();
    Some((org, domain))
}

fn state_str(s: OrgState) -> &'static str {
    match s { OrgState::Pending => "pending", OrgState::Proposed => "proposed", OrgState::Active => "active" }
}

fn proposal_state(s: ProposalState) -> &'static str {
    match s { ProposalState::Open => "open", ProposalState::Accepted => "accepted", ProposalState::Rejected => "rejected", ProposalState::UnderAudit => "under_audit", ProposalState::Blocked => "blocked" }
}

/// The document for the organisation's kind: the association constitution, or the co-op
/// Rules through the Rticles port. A company has no template in the reference (its
/// articles are held externally), so it renders nothing.
fn constitution_html(org: &Organisation, domain: &str) -> Option<String> {
    match org.kind {
        oneclick_core::org::Kind::Association => AssociationConstitution::from_org(org, domain).map(|c| render(&c)),
        oneclick_core::org::Kind::Coop => oneclick_core::render::coop_rules_html(org).ok(),
        oneclick_core::org::Kind::Company => None,
    }
}

fn proposal_json(org: &Organisation, p: &oneclick_core::proposals::Proposal, now: i64) -> Value {
    let tally = org.tally(p);
    let system = org.voting_system(p.kind.voting_role());
    json!({
        "slug": p.slug, "page": pages::proposal_slug(&p.slug), "title": p.title, "description": p.description,
        "kind": p.kind.class_name(), "parameters": serde_json::to_value(&p.kind).unwrap_or(Value::Null),
        "proposer": p.proposer, "creation_date": p.creation_date, "close_date": p.close_date,
        "state": proposal_state(p.state), "currently_open": p.currently_open(now),
        "votes_for": tally.votes_for, "votes_against": tally.votes_against, "member_count": tally.member_count,
        "abstained": tally.member_count - tally.total_votes(),
        "voting_system": system.map(|s| s.name()),
        "voting_system_description": system.map(|s| s.long_description(false)),
        "passing": system.map(|s| s.passed(&tally)),
        "can_close_early": system.map(|s| s.can_be_closed_early(&tally)),
        "votes": p.votes,
        "special": p.is_special(),
        "audit": p.audit,
        "seats": org.seats,
    })
}

pub fn get_route(site: &Site, path: &str) -> Option<Response> {
    let rest = path.strip_prefix(PREFIX)?;
    let Some((org, domain)) = fold(site) else {
        return Some(json(StatusCode::NOT_FOUND, json!({"error": "this site is not an organisation"})));
    };
    let now = now_ms();
    Some(match rest {
        "constitution.html" => match constitution_html(&org, &domain) {
            Some(h) => html(h),
            None => json(StatusCode::NOT_IMPLEMENTED, json!({"error": "no renderer for this kind yet"})),
        },
        "constitution.json" => json(StatusCode::OK, json!({
            "name": org.name(), "kind": org.kind.as_str(), "state": state_str(org.state),
            "updated_at": org.constitution_updated_at(), "domain": domain,
            "html": constitution_html(&org, &domain),
            "articles_url": org.clauses.get_text("articles_url"),
            "clauses": org.clauses.iter().map(|(n, v)| (n.clone(), serde_json::to_value(v).unwrap())).collect::<serde_json::Map<_, _>>(),
            "voting_period": org.voting_period(),
            "selectable_systems": oneclick_core::voting::SELECTABLE.iter().map(|s| json!({"name": s.name(), "description": s.description()})).collect::<Vec<_>>(),
            "period_options": oneclick_core::periods::PERIODS.iter().map(|p| json!({"seconds": p, "name": oneclick_core::periods::name_for_value(*p)})).collect::<Vec<_>>(),
        })),
        "clauses.json" => json(StatusCode::OK, Value::Object(org.clauses.iter().map(|(n, v)| {
            let mut o = serde_json::to_value(v).unwrap();
            o["started_at"] = json!(org.clauses.started_at(n));
            (n.clone(), o)
        }).collect())),
        "members.json" => json(StatusCode::OK, json!({
            "name": org.name(), "kind": org.kind.as_str(), "state": state_str(org.state),
            "can_hold_founding_vote": org.can_hold_founding_vote(),
            "coop_can_propose": org.coop_can_propose(),
            "audit_committee": org.clauses.get_boolean("audit_committee"),
            "enabled_groups": org.enabled_groups(),
            "seats": org.seats,
            "founding_vote_open": org.founding_proposal().map(|p| p.is_open()).unwrap_or(false),
            "member_classes": org.member_classes,
            "members": org.members.iter().map(|m| json!({
                "email": m.email, "first_name": m.first_name, "last_name": m.last_name, "name": m.name(),
                "member_class": m.member_class, "state": m.state.as_str(), "created_at": m.created_at,
                "stakeholder_group": m.stakeholder_group,
                "inducted_at": m.inducted_at, "elected_on": m.elected_on, "stood_down_on": m.stood_down_on,
                "transitions": m.transitions,
                "can_vote": org.has_permission(m, "vote"),
            })).collect::<Vec<_>>(),
        })),
        "proposals.json" => json(StatusCode::OK, json!({
            "state": state_str(org.state), "kind": org.kind.as_str(),
            "audit_committee": org.clauses.get_boolean("audit_committee"), "enabled_groups": org.enabled_groups(),
            "proposals": org.proposals.iter().rev().map(|p| proposal_json(&org, p, now)).collect::<Vec<_>>(),
        })),
        "decisions.json" => json(StatusCode::OK, json!({"decisions": org.decisions})),
        "fca.json" => match org.kind {
            oneclick_core::org::Kind::Coop => match oneclick_core::render::coop_fca_map(&org) {
                Ok(m) => json(StatusCode::OK, m),
                Err(e) => json(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e})),
            },
            _ => json(StatusCode::NOT_FOUND, json!({"error": "only a co-op has a registration form"})),
        },
        "voting-systems.json" => json(StatusCode::OK, oneclick_core::tables::voting_systems()),
        "lifecycles.json" => json(StatusCode::OK, oneclick_core::tables::lifecycles()),
        "state.json" => json(StatusCode::OK, serde_json::to_value(&org).unwrap_or(Value::Null)),
        r if r.starts_with("proposal/") && r.ends_with(".json") => {
            let slug = &r["proposal/".len()..r.len() - 5];
            match org.proposal(slug) {
                Some(p) => json(StatusCode::OK, proposal_json(&org, p, now)),
                None => json(StatusCode::NOT_FOUND, json!({"error": "no such proposal"})),
            }
        }
        _ => not_found("Page not found"),
    })
}

/// Apply the organisation's new state to the site: every changed page becomes journal
/// actions through `actions::apply` — `create` for a page that does not exist yet,
/// `add` for a new item, `edit` for a changed one. Returns what was written.
fn persist(site: &Site, org: &Organisation, domain: &str, now: i64, provenance: &str) -> Result<Vec<String>, String> {
    let mut written = Vec::new();
    for (slug, title, story) in pages::stories(org, domain) {
        match page_value(site, &slug) {
            None => {
                let action = json!({"type": "create", "id": pages::stable_id(&format!("create/{title}")), "item": {"title": title, "story": story}, "date": now, "provenance": provenance});
                match actions::apply(site, &slug, action) {
                    actions::Outcome::Ok => written.push(format!("create {slug}")),
                    actions::Outcome::Error(_, e) => return Err(format!("{slug}: {e}")),
                }
            }
            Some(existing) => {
                let old = existing["story"].as_array().cloned().unwrap_or_default();
                let (adds, edits) = pages::diff(&old, &story);
                for (item, after) in adds {
                    let action = json!({"type": "add", "id": item["id"], "item": item, "after": after, "date": now, "provenance": provenance});
                    if let actions::Outcome::Error(_, e) = actions::apply(site, &slug, action) { return Err(format!("{slug}: {e}")); }
                    written.push(format!("add {slug}"));
                }
                for item in edits {
                    let action = json!({"type": "edit", "id": item["id"], "item": item, "date": now, "provenance": provenance});
                    if let actions::Outcome::Error(_, e) = actions::apply(site, &slug, action) { return Err(format!("{slug}: {e}")); }
                    written.push(format!("edit {slug}"));
                }
            }
        }
    }
    Ok(written)
}

fn slugify(title: &str) -> String {
    let s = pages::as_slug(title);
    if s.is_empty() { "proposal".into() } else { s }
}

fn field<'a>(v: &'a Value, k: &str) -> &'a str {
    v[k].as_str().unwrap_or("")
}

pub fn post_route(site: &Site, path: &str, body: &[u8], authorized: bool) -> Option<Response> {
    let rest = path.strip_prefix(PREFIX)?;
    if rest != "command" && rest != "close" {
        return Some(not_found("Page not found"));
    }
    if !authorized {
        return Some(json(StatusCode::UNAUTHORIZED, json!({"error": "sign in as the site's owner to act"})));
    }
    let Some((mut org, domain)) = fold(site) else {
        return Some(json(StatusCode::NOT_FOUND, json!({"error": "this site is not an organisation"})));
    };
    let now = now_ms();
    let req: Value = if rest == "close" { json!({"command": "close"}) } else {
        match serde_json::from_slice(body) { Ok(v) => v, Err(e) => return Some(json(StatusCode::BAD_REQUEST, json!({"error": e.to_string()}))) }
    };
    let actor = field(&req, "actor").to_string();
    let command = field(&req, "command").to_string();
    let mut result = json!({});
    let outcome: Result<(), oneclick_core::org::DomainError> = match command.as_str() {
        "propose" => {
            let kind: ProposalKind = match req.get("kind") {
                Some(k) if k.is_object() => match serde_json::from_value(k.clone()) { Ok(k) => k, Err(e) => return Some(json(StatusCode::BAD_REQUEST, json!({"error": format!("bad kind: {e}")}))) },
                _ => ProposalKind::Freeform,
            };
            let title = field(&req, "title").to_string();
            let mut slug = slugify(&title);
            let mut n = 1;
            while org.proposal(&slug).is_some() { n += 1; slug = format!("{}-{n}", slugify(&title)); }
            org.propose(&slug, &title, field(&req, "description"), kind, &actor, now).map(|p| { result = json!({"slug": p.slug, "page": pages::proposal_slug(&p.slug)}); })
        }
        "found" => {
            let title = format!("Found {}", org.name());
            let mut slug = slugify(&title);
            let mut n = 1;
            while org.proposal(&slug).is_some() { n += 1; slug = format!("{}-{n}", slugify(&title)); }
            org.propose(&slug, &title, "The founding vote: at least three supporting votes over three days, every member counted.", ProposalKind::FoundAssociation, &actor, now)
                .map(|p| { result = json!({"slug": p.slug, "page": pages::proposal_slug(&p.slug)}); })
        }
        "vote" => org.vote(field(&req, "slug"), &actor, req["for"].as_bool().unwrap_or(true), now),
        "add-founding-member" => org.add_founding_member(field(&req, "email"), field(&req, "first_name"), field(&req, "last_name"), now),
        "member-event" => {
            let ev = match field(&req, "event") { "induct" => MemberEvent::Induct, "eject" => MemberEvent::Eject, "resign" => MemberEvent::Resign, "reactivate" => MemberEvent::Reactivate, _ => return Some(json(StatusCode::BAD_REQUEST, json!({"error": "event"}))) };
            org.member_event(field(&req, "email"), ev, now)
        }
        "close" => Ok(()),
        // the co-op's founding path: appoint classes while pending, propose when it can,
        // and the registrar's agent founds it (admin/coops_controller#found)
        "set-class" => org.set_member_class(field(&req, "email"), field(&req, "member_class")),
        "admit" => org.admit(field(&req, "email"), field(&req, "first_name"), field(&req, "last_name"), if field(&req, "member_class").is_empty() { "Member" } else { field(&req, "member_class") }, now),
        "coop-propose" => org.coop_propose(),
        // the company's doors (directors_controller, members_controller#update)
        "add-director" => {
            if !(req["certification"].as_bool().unwrap_or(false) && req["age_certification"].as_bool().unwrap_or(false)) {
                return Some(json(StatusCode::UNPROCESSABLE_ENTITY, json!({"error": "a director must accept both certifications"})));
            }
            org.add_director(field(&req, "email"), field(&req, "first_name"), field(&req, "last_name"), now)
        }
        "stand-down" => org.stand_down(field(&req, "email"), now),
        "edit-profile" => org.edit_profile(field(&req, "email"), field(&req, "first_name"), field(&req, "last_name")),
        "set-articles-url" => org.set_articles_url(field(&req, "url"), now),
        // the multi-stakeholder audit committee (audit.rs; Audit Committee Plan)
        "adopt-audit-committee" => org.adopt_audit_committee(req["adopt"].as_bool().unwrap_or(true), now),
        "set-group" => org.set_stakeholder_group(field(&req, "email"), field(&req, "group")),
        "appoint-representative" => org.appoint_representative(field(&req, "group"), field(&req, "email")),
        "audit" => org.audit_decision(field(&req, "slug"), &actor, req["approve"].as_bool().unwrap_or(false), now).map(|st| { result = json!({"audit": st}); }),
        "coop-found" => org.coop_found(),
        _ => return Some(json(StatusCode::BAD_REQUEST, json!({"error": format!("unknown command {command}")}))),
    };
    if let Err(e) = outcome {
        return Some(json(StatusCode::UNPROCESSABLE_ENTITY, json!({"error": e.to_string()})));
    }
    // the reference's PeriodicProposalCloser, run on every touch instead of every 60 s
    let closed = org.close_proposals(now);
    let provenance = format!("oneclick {command}{}", if actor.is_empty() { String::new() } else { format!(" by {actor}") });
    match persist(site, &org, &domain, now, &provenance) {
        Ok(written) => Some(json(StatusCode::OK, json!({"ok": true, "command": command, "result": result, "closed": closed, "written": written, "state": state_str(org.state)}))),
        Err(e) => Some(json(StatusCode::INTERNAL_SERVER_ERROR, json!({"error": e}))),
    }
}

#[allow(dead_code)]
fn _roles() -> Role { Role::General }
