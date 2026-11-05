//! The group rule behind `feishu.allowed_users`: a group is admitted when a listed person is one of its members. Intake
//! and reply both call `chat_has_owner`, which reads the group's member list fresh on every call and keeps nothing.

use std::collections::BTreeMap;

use anyhow::Context;

use crate::client::{FeishuClient, Page};

/// Whether a person in `owners` (the `feishu.allowed_users` map, open_id → name) is a member of `chat_id`. Every
/// page of the member list is read, so a failed page fails the whole check.
pub async fn chat_has_owner(
  client: &FeishuClient,
  owners: &BTreeMap<String, String>,
  chat_id: &str,
) -> anyhow::Result<bool> {
  let mut has_owner = false;
  let mut page_token = None;
  loop {
    let Page { items, next_page_token } = client.chat_members(chat_id, page_token.as_deref()).await?;
    for item in &items {
      let member_id = item["member_id"].as_str().context("a chat member has no member_id")?;
      has_owner |= owners.contains_key(member_id);
    }
    match next_page_token {
      Some(token) => page_token = Some(token),
      None => return Ok(has_owner),
    }
  }
}

#[cfg(test)]
mod tests {
  use charliebot_compat::http_client;
  use serde_json::json;

  use super::*;
  use crate::test_support::{Answer, serve};

  const OWNER: &str = "ou_synthetic_owner";
  const MEMBER: &str = "ou_synthetic_member";
  const CHAT: &str = "oc_synthetic_owners";

  fn owners() -> BTreeMap<String, String> {
    BTreeMap::from([(OWNER.to_string(), "Owner".to_string())])
  }

  fn client(base_url: &str, app_id: &str) -> FeishuClient {
    let http = http_client::client(&http_client::Settings::from_env()).expect("the HTTP client builds");
    FeishuClient::new(http, base_url, app_id, "synthetic-secret")
  }

  fn member(open_id: &str) -> serde_json::Value {
    json!({"member_id": open_id, "member_id_type": "open_id", "name": "synthetic"})
  }

  #[tokio::test]
  async fn a_listed_person_among_the_members_admits_the_chat() {
    let page = json!({"code": 0, "data": {"items": [member(MEMBER), member(OWNER)], "has_more": false}});
    let stub = serve(move |_| Answer::json(page.clone())).await;
    assert!(chat_has_owner(&client(&stub.base_url, "cli_owner_present"), &owners(), CHAT).await.unwrap());
  }

  #[tokio::test]
  async fn a_chat_with_no_listed_person_is_not_admitted() {
    let page = json!({"code": 0, "data": {"items": [member(MEMBER)], "has_more": false}});
    let stub = serve(move |_| Answer::json(page.clone())).await;
    assert!(!chat_has_owner(&client(&stub.base_url, "cli_owner_absent"), &owners(), CHAT).await.unwrap());
  }

  #[tokio::test]
  async fn every_page_of_the_member_list_is_read() {
    let stub = serve(|request| {
      if request.target.contains("page_token=p2") {
        Answer::json(json!({"code": 0, "data": {"items": [member(OWNER)], "has_more": false}}))
      } else {
        Answer::json(json!({"code": 0, "data": {"items": [member(MEMBER)], "has_more": true, "page_token": "p2"}}))
      }
    })
    .await;
    assert!(chat_has_owner(&client(&stub.base_url, "cli_owner_paging"), &owners(), CHAT).await.unwrap());
    let requests = stub.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
      requests[0].target,
      "/open-apis/im/v1/chats/oc_synthetic_owners/members?member_id_type=open_id&page_size=50"
    );
    assert!(requests[1].target.contains("page_token=p2"));
  }

  #[tokio::test]
  async fn a_member_list_error_fails_the_check() {
    let stub = serve(|_| Answer::json(json!({"code": 99991672, "msg": "no permission"}))).await;
    assert!(chat_has_owner(&client(&stub.base_url, "cli_owner_error"), &owners(), CHAT).await.is_err());
  }
}
