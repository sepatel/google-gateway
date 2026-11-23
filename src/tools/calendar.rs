use chrono::{SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{account_for, compact, path_id, run};
use crate::app::AppState;
use crate::error::GatewayResult;
use crate::google::Service;
use crate::google::api::Api;
use crate::mail::{html_to_text, truncate};

const LIST_DESCRIPTION_LIMIT: usize = 500;
const EVENT_DESCRIPTION_LIMIT: usize = 20_000;
const ATTENDEE_LIMIT: usize = 30;

pub fn descriptors(account: &Value) -> Vec<Value> {
    let calendar_id = json!({
        "type": "string",
        "description": "Calendar ID from calendar_list_calendars. Default \"primary\"."
    });
    let time = |what: &str| {
        json!({
            "type": "string",
            "description": format!("{what}, RFC 3339 with an offset, e.g. 2026-11-23T09:00:00-05:00."),
        })
    };
    vec![
        json!({
            "name": "calendar_list_calendars",
            "description": "List the calendars this account can see, with their IDs, time zones and access level.",
            "inputSchema": {"type": "object", "properties": {"account": account}},
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "calendar_list_events",
            "description": "List events in time order, with recurring events expanded into individual occurrences. Descriptions are shortened; use calendar_get_event for one event in full.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "calendar_id": calendar_id,
                    "time_min": time("Earliest end time to include. Default now"),
                    "time_max": time("Latest start time to include. Default unbounded"),
                    "query": {"type": "string", "description": "Free-text search over titles, descriptions, locations and attendees."},
                    "max_results": {"type": "integer", "description": "1-250, default 50."},
                    "page_token": {"type": "string", "description": "next_page_token from a previous call."},
                },
            },
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "calendar_get_event",
            "description": "Get one event with its full description and attendee list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "calendar_id": calendar_id,
                    "event_id": {"type": "string"},
                },
                "required": ["event_id"],
            },
            "annotations": {"readOnlyHint": true},
        }),
        json!({
            "name": "calendar_free_busy",
            "description": "Busy time blocks for one or more calendars, including colleagues' calendars where the organisation shares availability.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "account": account,
                    "time_min": time("Start of the window"),
                    "time_max": time("End of the window"),
                    "calendars": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Calendar IDs or email addresses. Default [\"primary\"].",
                    },
                },
                "required": ["time_min", "time_max"],
            },
            "annotations": {"readOnlyHint": true},
        }),
    ]
}

pub async fn call(state: &AppState, name: &str, args: Value) -> Option<GatewayResult<Value>> {
    Some(match name {
        "calendar_list_calendars" => run(args, |a| list_calendars(state, a)).await,
        "calendar_list_events" => run(args, |a| list_events(state, a)).await,
        "calendar_get_event" => run(args, |a| get_event(state, a)).await,
        "calendar_free_busy" => run(args, |a| free_busy(state, a)).await,
        _ => return None,
    })
}

#[derive(Deserialize)]
struct AccountArgs {
    account: Option<String>,
}

async fn list_calendars(state: &AppState, args: AccountArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.account, Service::Calendar).await?;
    let response = state
        .google
        .get(
            &account,
            Api::Calendar,
            &["users", "me", "calendarList"],
            &[("maxResults", "250")],
        )
        .await?;
    let calendars: Vec<Value> = response["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| {
            compact(json!({
                "id": c["id"],
                "name": c["summaryOverride"].as_str().or(c["summary"].as_str()),
                "description": c["description"],
                "primary": c["primary"],
                "time_zone": c["timeZone"],
                "access_role": c["accessRole"],
            }))
        })
        .collect();
    Ok(json!({ "account": account, "calendars": calendars }))
}

#[derive(Deserialize)]
struct ListEventsArgs {
    account: Option<String>,
    calendar_id: Option<String>,
    time_min: Option<String>,
    time_max: Option<String>,
    query: Option<String>,
    max_results: Option<u32>,
    page_token: Option<String>,
}

async fn list_events(state: &AppState, args: ListEventsArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.account, Service::Calendar).await?;
    let calendar_id = args.calendar_id.unwrap_or_else(|| "primary".into());
    let time_min = args
        .time_min
        .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true));
    let max_results = args.max_results.unwrap_or(50).clamp(1, 250).to_string();

    let mut query = vec![
        ("singleEvents", "true"),
        ("orderBy", "startTime"),
        ("timeMin", time_min.as_str()),
        ("maxResults", max_results.as_str()),
    ];
    for (key, value) in [
        ("timeMax", &args.time_max),
        ("q", &args.query),
        ("pageToken", &args.page_token),
    ] {
        if let Some(value) = value.as_deref() {
            query.push((key, value));
        }
    }

    let response = state
        .google
        .get(
            &account,
            Api::Calendar,
            &["calendars", path_id("calendar ID", &calendar_id)?, "events"],
            &query,
        )
        .await?;
    let events: Vec<Value> = response["items"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| render_event(e, LIST_DESCRIPTION_LIMIT))
        .collect();
    Ok(compact(json!({
        "account": account,
        "calendar_id": calendar_id,
        "time_zone": response["timeZone"],
        "events": events,
        "next_page_token": response["nextPageToken"],
    })))
}

#[derive(Deserialize)]
struct GetEventArgs {
    account: Option<String>,
    calendar_id: Option<String>,
    event_id: String,
}

async fn get_event(state: &AppState, args: GetEventArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.account, Service::Calendar).await?;
    let calendar_id = args.calendar_id.unwrap_or_else(|| "primary".into());
    let event = state
        .google
        .get(
            &account,
            Api::Calendar,
            &[
                "calendars",
                path_id("calendar ID", &calendar_id)?,
                "events",
                path_id("event ID", &args.event_id)?,
            ],
            &[],
        )
        .await?;
    let mut rendered = render_event(&event, EVENT_DESCRIPTION_LIMIT);
    rendered["account"] = json!(account);
    Ok(rendered)
}

#[derive(Deserialize)]
struct FreeBusyArgs {
    account: Option<String>,
    time_min: String,
    time_max: String,
    calendars: Option<Vec<String>>,
}

async fn free_busy(state: &AppState, args: FreeBusyArgs) -> GatewayResult<Value> {
    let account = account_for(state, args.account, Service::Calendar).await?;
    let calendars = args.calendars.unwrap_or_else(|| vec!["primary".into()]);
    let body = json!({
        "timeMin": args.time_min,
        "timeMax": args.time_max,
        "items": calendars.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
    });
    let response = state
        .google
        .post(&account, Api::Calendar, &["freeBusy"], &body)
        .await?;
    let calendars: serde_json::Map<String, Value> = response["calendars"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(id, c)| {
            let errors: Vec<&Value> = c["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|e| &e["reason"])
                .collect();
            let entry = if errors.is_empty() {
                json!({ "busy": c["busy"] })
            } else {
                json!({ "busy": c["busy"], "errors": errors })
            };
            (id.clone(), entry)
        })
        .collect();
    Ok(json!({
        "account": account,
        "time_min": response["timeMin"],
        "time_max": response["timeMax"],
        "calendars": calendars,
    }))
}

fn render_event(event: &Value, description_limit: usize) -> Value {
    let start = &event["start"];
    let attendees: Vec<&Value> = event["attendees"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let my_response = attendees
        .iter()
        .find(|a| a["self"] == true)
        .map(|a| a["responseStatus"].clone());
    let meeting_link = event["hangoutLink"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            event["conferenceData"]["entryPoints"]
                .as_array()?
                .iter()
                .find(|p| p["entryPointType"] == "video")?["uri"]
                .as_str()
                .map(str::to_string)
        });

    let description = event["description"].as_str().map(|d| {
        // Calendar stores descriptions written in its editor as HTML.
        let text = if d.contains('<') {
            html_to_text(d)
        } else {
            d.to_string()
        };
        truncate(&text, description_limit).0
    });

    let shown: Vec<Value> = attendees
        .iter()
        .take(ATTENDEE_LIMIT)
        .map(|a| {
            compact(json!({
                "email": a["email"],
                "name": a["displayName"],
                "response": a["responseStatus"],
                "optional": a["optional"],
                "organizer": a["organizer"],
            }))
        })
        .collect();

    let mut rendered = compact(json!({
        "id": event["id"],
        "status": event["status"],
        "summary": event["summary"],
        "start": start["dateTime"].as_str().or(start["date"].as_str()),
        "end": event["end"]["dateTime"].as_str().or(event["end"]["date"].as_str()),
        "all_day": start["date"].is_string(),
        "location": event["location"],
        "description": description,
        "organizer": event["organizer"]["email"],
        "my_response": my_response,
        "meeting_link": meeting_link,
        "recurring_event_id": event["recurringEventId"],
        "html_link": event["htmlLink"],
    }));
    if !shown.is_empty() {
        rendered["attendees"] = json!(shown);
    }
    if attendees.len() > ATTENDEE_LIMIT {
        rendered["attendees_omitted"] = json!(attendees.len() - ATTENDEE_LIMIT);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_day_events_my_response_and_the_video_link() {
        let event = json!({
            "id": "e1",
            "summary": "GovCloud Sync",
            "start": {"date": "2026-11-27"},
            "end": {"date": "2026-11-28"},
            "description": "<p>Agenda&nbsp;below</p>",
            "attendees": [
                {"email": "me@example.com", "self": true, "responseStatus": "accepted"},
                {"email": "vaughn@example.com", "responseStatus": "declined"},
            ],
            "conferenceData": {"entryPoints": [
                {"entryPointType": "phone", "uri": "tel:+1"},
                {"entryPointType": "video", "uri": "https://meet.google.com/abc"},
            ]},
        });
        let rendered = render_event(&event, 100);
        assert_eq!(rendered["all_day"], true);
        assert_eq!(rendered["start"], "2026-11-27");
        assert_eq!(rendered["my_response"], "accepted");
        assert_eq!(rendered["meeting_link"], "https://meet.google.com/abc");
        assert_eq!(rendered["description"], "Agenda below");
        assert_eq!(rendered["attendees"][1]["response"], "declined");
    }
}
