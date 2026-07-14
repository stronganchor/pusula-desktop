use std::{cmp::Ordering, collections::HashMap};

use chrono::Local;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::Deserialize;
use serde_json::{json, Map, Number, Value};
use url::Url;

use crate::{
    db::{
        mark_modified, read_business_profile, validate_iso_date, validate_profile,
        write_business_profile, Database, MAX_SAFE_JS_INTEGER,
    },
    error::{AppError, AppResult},
    models::BusinessProfile,
};

#[derive(Debug, Clone, Default, Deserialize)]
struct ContactInput {
    #[serde(default)]
    name: String,
    #[serde(default)]
    phone: String,
    #[serde(default)]
    home_address: String,
    #[serde(default)]
    work_address: String,
}

#[derive(Debug, Clone)]
struct NewInstallment {
    due_date: Option<String>,
    amount_kurus: i64,
    paid: bool,
    payment_date: Option<String>,
}

type InstallmentDbRow = (i64, i64, Option<String>, i64, Option<String>);

struct ParsedRequest {
    segments: Vec<String>,
    query: HashMap<String, String>,
}

impl Database {
    pub fn api_request(
        &self,
        path: &str,
        method: Option<&str>,
        body: Option<Value>,
    ) -> AppResult<Value> {
        let request = parse_request(path)?;
        let route: Vec<&str> = request.segments.iter().map(String::as_str).collect();
        let method = method.unwrap_or("GET").to_ascii_uppercase();
        let body = body.unwrap_or(Value::Null);

        match (method.as_str(), route.as_slice()) {
            ("GET", ["customers"]) => self.list_customers(&request.query),
            ("POST", ["customers"]) => self.create_customer(&body),
            ("GET", ["customers", "next-id"]) => self.next_customer_id(),
            ("GET", ["customers", id]) => self.get_customer(parse_id(id, "müşteri")?),
            ("PUT" | "PATCH", ["customers", id]) => {
                self.update_customer(parse_id(id, "müşteri")?, &body)
            }
            ("DELETE", ["customers", id]) => self.delete_customer(parse_id(id, "müşteri")?),
            ("GET", ["customers", id, "contacts"]) => self.list_contacts(parse_id(id, "müşteri")?),
            ("POST", ["customers", id, "contacts"]) => {
                self.create_contact(parse_id(id, "müşteri")?, &body)
            }
            ("PUT" | "PATCH", ["customers", id, "contacts"]) => {
                self.replace_contacts(parse_id(id, "müşteri")?, &body)
            }
            ("GET", ["customers", customer_id, "contacts", contact_id]) => self.get_contact(
                parse_id(customer_id, "müşteri")?,
                parse_id(contact_id, "iletişim")?,
            ),
            ("PUT" | "PATCH", ["customers", customer_id, "contacts", contact_id]) => self
                .update_contact(
                    parse_id(customer_id, "müşteri")?,
                    parse_id(contact_id, "iletişim")?,
                    &body,
                ),
            ("DELETE", ["customers", customer_id, "contacts", contact_id]) => self.delete_contact(
                parse_id(customer_id, "müşteri")?,
                parse_id(contact_id, "iletişim")?,
            ),
            ("GET", ["sales"]) => self.list_sales(&request.query),
            ("POST", ["sales"]) => self.create_sale(&body),
            ("GET", ["sales", id]) => self.get_sale(parse_id(id, "satış")?),
            ("PUT" | "PATCH", ["sales", id]) => self.update_sale(parse_id(id, "satış")?, &body),
            ("DELETE", ["sales", id]) => self.delete_sale(parse_id(id, "satış")?),
            ("GET", ["installments"]) => self.list_installments(&request.query),
            ("POST", ["installments"]) => self.create_installment(&body),
            ("GET", ["installments", id]) => self.get_installment(parse_id(id, "taksit")?),
            ("PUT" | "PATCH", ["installments", id]) => {
                self.update_installment(parse_id(id, "taksit")?, &body)
            }
            ("DELETE", ["installments", id]) => self.delete_installment(parse_id(id, "taksit")?),
            ("GET", ["installments", id, "payments"]) => {
                self.list_payments(parse_id(id, "taksit")?)
            }
            ("POST", ["installments", id, "payments"]) => {
                self.create_payment(parse_id(id, "taksit")?, &body)
            }
            ("GET", ["installments", id, "payments", payment_id]) => {
                self.get_payment(parse_id(id, "taksit")?, parse_id(payment_id, "ödeme")?)
            }
            ("PUT" | "PATCH", ["installments", id, "payments", payment_id]) => self.update_payment(
                parse_id(id, "taksit")?,
                parse_id(payment_id, "ödeme")?,
                &body,
            ),
            ("DELETE", ["installments", id, "payments", payment_id]) => {
                self.delete_payment(parse_id(id, "taksit")?, parse_id(payment_id, "ödeme")?)
            }
            ("GET", ["daily-report"]) => self.daily_report(&request.query),
            ("GET", ["expected-payments"]) => self.expected_payments(&request.query),
            ("GET", ["business-profile"]) => self.get_business_profile_json(),
            ("PUT" | "PATCH", ["business-profile"]) => self.update_business_profile(&body),
            ("GET", ["offline-snapshot"]) => self.offline_snapshot(),
            _ => Err(AppError::user(format!(
                "Desteklenmeyen işlem: {method} {path}"
            ))),
        }
    }
}

fn parse_request(path: &str) -> AppResult<ParsedRequest> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(AppError::user("İstek yolu / ile başlamalıdır."));
    }
    let url = Url::parse(&format!("http://pusula.local{path}"))?;
    let segments = url
        .path_segments()
        .map(|parts| {
            parts
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let query = url.query_pairs().into_owned().collect();
    Ok(ParsedRequest { segments, query })
}

fn parse_id(value: &str, label: &str) -> AppResult<i64> {
    let id = value
        .parse::<i64>()
        .map_err(|_| AppError::user(format!("Geçersiz {label} numarası.")))?;
    if id <= 0 || id > MAX_SAFE_JS_INTEGER {
        return Err(AppError::user(format!("Geçersiz {label} numarası.")));
    }
    Ok(id)
}

fn object(value: &Value) -> AppResult<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| AppError::user("İstek gövdesi bir JSON nesnesi olmalıdır."))
}

fn text_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).map(|value| match value {
        Value::String(text) => text.trim().to_owned(),
        Value::Null => String::new(),
        other => other.to_string().trim_matches('"').trim().to_owned(),
    })
}

fn i64_field(map: &Map<String, Value>, key: &str) -> AppResult<Option<i64>> {
    let Some(value) = map.get(key) else {
        return Ok(None);
    };
    let parsed = match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    };
    let parsed = parsed.ok_or_else(|| AppError::user(format!("{key} tam sayı olmalıdır.")))?;
    if parsed.unsigned_abs() > MAX_SAFE_JS_INTEGER as u64 {
        return Err(AppError::user(format!(
            "{key} güvenli uygulama aralığını aşıyor."
        )));
    }
    Ok(Some(parsed))
}

fn bool_field(map: &Map<String, Value>, key: &str) -> Option<bool> {
    map.get(key).and_then(|value| match value {
        Value::Bool(value) => Some(*value),
        Value::Number(value) => value.as_i64().map(|value| value == 1),
        Value::String(value) => Some(matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes"
        )),
        _ => None,
    })
}

pub(crate) fn money_to_kurus(value: &Value) -> AppResult<i64> {
    let raw = match value {
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.trim().replace(',', "."),
        _ => return Err(AppError::user("Para tutarı sayı olmalıdır.")),
    };
    parse_decimal_kurus(&raw)
}

fn parse_decimal_kurus(raw: &str) -> AppResult<i64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(AppError::user("Para tutarı boş olamaz."));
    }

    if raw.contains(['e', 'E']) {
        return Err(AppError::user(
            "Para tutarında bilimsel gösterim kullanılamaz.",
        ));
    }

    let (negative, unsigned) = raw
        .strip_prefix('-')
        .map(|value| (true, value))
        .unwrap_or((false, raw));
    let unsigned = unsigned.strip_prefix('+').unwrap_or(unsigned);
    let mut parts = unsigned.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(AppError::user("Para tutarı geçersiz."));
    }

    let whole = whole
        .parse::<i64>()
        .map_err(|_| AppError::user("Para tutarı desteklenen aralığı aşıyor."))?;
    let mut digits = fraction.bytes().map(|byte| i64::from(byte - b'0'));
    let first = digits.next().unwrap_or(0);
    let second = digits.next().unwrap_or(0);
    let round_up = digits.next().unwrap_or(0) >= 5;
    let mut result = whole
        .checked_mul(100)
        .and_then(|value| value.checked_add(first * 10 + second))
        .and_then(|value| value.checked_add(i64::from(round_up)))
        .ok_or_else(|| AppError::user("Para tutarı desteklenen aralığı aşıyor."))?;
    if negative {
        result = result
            .checked_neg()
            .ok_or_else(|| AppError::user("Para tutarı desteklenen aralığı aşıyor."))?;
    }
    if result.unsigned_abs() > MAX_SAFE_JS_INTEGER as u64 {
        return Err(AppError::user(
            "Para tutarı güvenli uygulama aralığını aşıyor.",
        ));
    }
    Ok(result)
}

fn money_json(kurus: i64) -> Value {
    Value::Number(Number::from_f64(kurus as f64 / 100.0).unwrap_or_else(|| Number::from(0)))
}

fn today() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

fn created_at() -> String {
    Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn require_date(value: Option<String>, fallback_today: bool, label: &str) -> AppResult<String> {
    let value = value
        .filter(|value| !value.is_empty())
        .or_else(|| fallback_today.then(today))
        .ok_or_else(|| AppError::user(format!("{label} zorunludur.")))?;
    validate_iso_date(&value, label)?;
    Ok(value)
}

fn date_in_range(date: &str, query: &HashMap<String, String>) -> bool {
    query
        .get("start")
        .is_none_or(|start| date >= start.as_str())
        && query.get("end").is_none_or(|end| date <= end.as_str())
}

fn query_flag(query: &HashMap<String, String>, name: &str) -> bool {
    query
        .get("with")
        .is_some_and(|value| value.split(',').any(|item| item.trim() == name))
}

impl Database {
    fn next_customer_id(&self) -> AppResult<Value> {
        let connection = self.connect()?;
        Ok(json!({ "next_id": lowest_available_customer_id(&connection)? }))
    }

    fn list_customers(&self, query: &HashMap<String, String>) -> AppResult<Value> {
        let connection = self.connect()?;
        let include_contacts = query_flag(query, "contacts");
        let include_late = query_flag(query, "late_unpaid");
        let search = query.get("search").map(|value| value.to_lowercase());
        let name = query.get("name").map(|value| value.to_lowercase());
        let phone = query.get("phone").map(|value| value.to_lowercase());
        let address = query.get("address").map(|value| value.to_lowercase());
        let id = query.get("id").and_then(|value| value.parse::<i64>().ok());
        let limit = query
            .get("limit")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(100)
            .clamp(1, 500);
        let offset = query
            .get("offset")
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);

        let mut rows = customer_bases(&connection)?;
        rows.retain(|row| {
            let object = row.as_object().expect("customer row must be an object");
            let text = |key: &str| {
                object
                    .get(key)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase()
            };
            id.is_none_or(|expected| object["id"].as_i64() == Some(expected))
                && search.as_ref().is_none_or(|needle| {
                    ["name", "phone", "address", "work_address"]
                        .iter()
                        .any(|key| text(key).contains(needle))
                })
                && name
                    .as_ref()
                    .is_none_or(|needle| text("name").contains(needle))
                && phone
                    .as_ref()
                    .is_none_or(|needle| text("phone").contains(needle))
                && address.as_ref().is_none_or(|needle| {
                    text("address").contains(needle) || text("work_address").contains(needle)
                })
        });

        let mut selected: Vec<Value> = rows.into_iter().skip(offset).take(limit).collect();
        for row in &mut selected {
            enrich_customer(&connection, row, include_contacts, include_late)?;
        }
        Ok(Value::Array(selected))
    }

    fn get_customer(&self, id: i64) -> AppResult<Value> {
        let connection = self.connect()?;
        let mut customer =
            customer_base(&connection, id)?.ok_or_else(|| AppError::user("Müşteri bulunamadı."))?;
        enrich_customer(&connection, &mut customer, true, false)?;
        Ok(customer)
    }

    fn create_customer(&self, body: &Value) -> AppResult<Value> {
        let body = object(body)?;
        let name = text_field(body, "name").unwrap_or_default();
        if name.is_empty() {
            return Err(AppError::user("Müşteri adı zorunludur."));
        }
        validate_text_length("Müşteri adı", &name, 120)?;
        let phone = text_field(body, "phone").unwrap_or_default();
        let address = text_field(body, "address").unwrap_or_default();
        let work_address = text_field(body, "work_address").unwrap_or_default();
        let notes = text_field(body, "notes").unwrap_or_default();
        validate_customer_text(&phone, &address, &work_address, &notes)?;
        let registration_date = require_date(
            text_field(body, "registration_date"),
            true,
            "Müşteri kayıt tarihi",
        )?;
        let contacts = body.get("contacts").map(normalize_contacts).transpose()?;

        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let requested_id = i64_field(body, "id")?.filter(|id| *id > 0);
        let customer_id = requested_id.unwrap_or(lowest_available_customer_id(&transaction)?);

        let existing: Option<(String, String, String, String, String, String)> = transaction
            .query_row(
                "SELECT name, phone, address, work_address, notes, registration_date FROM customers WHERE id = ?",
                [customer_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
            )
            .optional()?;
        if let Some(existing) = existing {
            let matches = existing
                == (
                    name.clone(),
                    phone.clone(),
                    address.clone(),
                    work_address.clone(),
                    notes.clone(),
                    registration_date.clone(),
                );
            if !matches {
                return Err(AppError::user(
                    "Bu müşteri numarası zaten kullanılıyor. Listeyi yenileyip tekrar deneyin.",
                ));
            }
            if let Some(contacts) = contacts.as_ref().filter(|rows| !rows.is_empty()) {
                replace_contacts_tx(&transaction, customer_id, contacts)?;
                mark_modified(&transaction)?;
            }
            transaction.commit()?;
            return Ok(json!({ "id": customer_id }));
        }

        transaction.execute(
            "INSERT INTO customers(id, name, phone, address, work_address, notes, registration_date) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![customer_id, name, phone, address, work_address, notes, registration_date],
        )?;
        if let Some(contacts) = contacts {
            replace_contacts_tx(&transaction, customer_id, &contacts)?;
        }
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "id": customer_id }))
    }

    fn update_customer(&self, id: i64, body: &Value) -> AppResult<Value> {
        let body = object(body)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let current: Option<(String, String, String, String, String)> = transaction
            .query_row(
                "SELECT name, phone, address, work_address, notes FROM customers WHERE id = ?",
                [id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .optional()?;
        let Some(current) = current else {
            return Err(AppError::user("Müşteri bulunamadı."));
        };

        let name = text_field(body, "name").unwrap_or(current.0);
        let phone = text_field(body, "phone").unwrap_or(current.1);
        let address = text_field(body, "address").unwrap_or(current.2);
        let work_address = text_field(body, "work_address").unwrap_or(current.3);
        let notes = text_field(body, "notes").unwrap_or(current.4);
        if name.is_empty() {
            return Err(AppError::user("Müşteri adı zorunludur."));
        }
        validate_text_length("Müşteri adı", &name, 120)?;
        validate_customer_text(&phone, &address, &work_address, &notes)?;

        transaction.execute(
            "UPDATE customers SET name = ?, phone = ?, address = ?, work_address = ?, notes = ? WHERE id = ?",
            params![name, phone, address, work_address, notes, id],
        )?;
        if let Some(value) = body.get("contacts") {
            replace_contacts_tx(&transaction, id, &normalize_contacts(value)?)?;
        }
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "updated": true }))
    }

    fn delete_customer(&self, id: i64) -> AppResult<Value> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        if transaction.execute("DELETE FROM customers WHERE id = ?", [id])? == 0 {
            return Err(AppError::user("Müşteri bulunamadı."));
        }
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "deleted": true }))
    }

    fn list_contacts(&self, customer_id: i64) -> AppResult<Value> {
        let connection = self.connect()?;
        ensure_customer_exists(&connection, customer_id)?;
        Ok(Value::Array(contacts_for_customer(
            &connection,
            customer_id,
        )?))
    }

    fn get_contact(&self, customer_id: i64, contact_id: i64) -> AppResult<Value> {
        let connection = self.connect()?;
        contact_json(&connection, customer_id, contact_id)?
            .ok_or_else(|| AppError::user("İletişim kaydı bulunamadı."))
    }

    fn create_contact(&self, customer_id: i64, body: &Value) -> AppResult<Value> {
        let contact = normalize_contact(body)?
            .ok_or_else(|| AppError::user("Boş iletişim kaydı oluşturulamaz."))?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        ensure_customer_exists(&transaction, customer_id)?;
        transaction.execute(
            "INSERT INTO contacts(customer_id, name, phone, home_address, work_address) VALUES (?, ?, ?, ?, ?)",
            params![customer_id, contact.name, contact.phone, contact.home_address, contact.work_address],
        )?;
        let id = transaction.last_insert_rowid();
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "id": id }))
    }

    fn replace_contacts(&self, customer_id: i64, body: &Value) -> AppResult<Value> {
        let contacts = normalize_contacts(body)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        ensure_customer_exists(&transaction, customer_id)?;
        replace_contacts_tx(&transaction, customer_id, &contacts)?;
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "saved": true }))
    }

    fn update_contact(&self, customer_id: i64, contact_id: i64, body: &Value) -> AppResult<Value> {
        let body = object(body)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let current = contact_json(&transaction, customer_id, contact_id)?
            .ok_or_else(|| AppError::user("İletişim kaydı bulunamadı."))?;
        let current = current.as_object().expect("contact row must be an object");
        let contact = ContactInput {
            name: text_field(body, "name").unwrap_or_else(|| value_text(current, "name")),
            phone: text_field(body, "phone").unwrap_or_else(|| value_text(current, "phone")),
            home_address: text_field(body, "home_address")
                .unwrap_or_else(|| value_text(current, "home_address")),
            work_address: text_field(body, "work_address")
                .unwrap_or_else(|| value_text(current, "work_address")),
        };
        validate_contact(&contact)?;
        transaction.execute(
            "UPDATE contacts SET name = ?, phone = ?, home_address = ?, work_address = ? WHERE id = ? AND customer_id = ?",
            params![contact.name, contact.phone, contact.home_address, contact.work_address, contact_id, customer_id],
        )?;
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "updated": true }))
    }

    fn delete_contact(&self, customer_id: i64, contact_id: i64) -> AppResult<Value> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        if transaction.execute(
            "DELETE FROM contacts WHERE id = ? AND customer_id = ?",
            params![contact_id, customer_id],
        )? == 0
        {
            return Err(AppError::user("İletişim kaydı bulunamadı."));
        }
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "deleted": true }))
    }
}

fn customer_bases(connection: &Connection) -> AppResult<Vec<Value>> {
    let mut statement = connection.prepare(
        "SELECT id, name, phone, address, work_address, notes, registration_date
         FROM customers ORDER BY registration_date DESC, id DESC",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(json!({
            "id": row.get::<_, i64>(0)?,
            "name": row.get::<_, String>(1)?,
            "phone": row.get::<_, String>(2)?,
            "address": row.get::<_, String>(3)?,
            "work_address": row.get::<_, String>(4)?,
            "notes": row.get::<_, String>(5)?,
            "registration_date": row.get::<_, String>(6)?,
        }))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn customer_base(connection: &Connection, id: i64) -> AppResult<Option<Value>> {
    Ok(connection
        .query_row(
            "SELECT id, name, phone, address, work_address, notes, registration_date FROM customers WHERE id = ?",
            [id],
            |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "phone": row.get::<_, String>(2)?,
                    "address": row.get::<_, String>(3)?,
                    "work_address": row.get::<_, String>(4)?,
                    "notes": row.get::<_, String>(5)?,
                    "registration_date": row.get::<_, String>(6)?,
                }))
            },
        )
        .optional()?)
}

fn enrich_customer(
    connection: &Connection,
    customer: &mut Value,
    include_contacts: bool,
    include_late: bool,
) -> AppResult<()> {
    let customer_id = customer["id"]
        .as_i64()
        .ok_or_else(|| AppError::user("Müşteri numarası okunamadı."))?;
    let debt: i64 = connection.query_row(
        "SELECT COALESCE(SUM(CASE WHEN i.amount_kurus > COALESCE(p.paid_kurus, 0)
             THEN i.amount_kurus - COALESCE(p.paid_kurus, 0) ELSE 0 END), 0)
         FROM sales s
         JOIN installments i ON i.sale_id = s.id
         LEFT JOIN (
             SELECT installment_id, SUM(amount_kurus) AS paid_kurus
             FROM installment_payments GROUP BY installment_id
         ) p ON p.installment_id = i.id
         WHERE s.customer_id = ?",
        [customer_id],
        |row| row.get(0),
    )?;
    let object = customer
        .as_object_mut()
        .ok_or_else(|| AppError::user("Müşteri kaydı geçersiz."))?;
    object.insert("debt_total".to_owned(), money_json(debt));
    if include_contacts {
        object.insert(
            "contacts".to_owned(),
            Value::Array(contacts_for_customer(connection, customer_id)?),
        );
    }
    if include_late {
        let late: bool = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sales s
                 JOIN installments i ON i.sale_id = s.id
                 LEFT JOIN (
                     SELECT installment_id, SUM(amount_kurus) AS paid_kurus
                     FROM installment_payments GROUP BY installment_id
                 ) p ON p.installment_id = i.id
                 WHERE s.customer_id = ? AND i.due_date IS NOT NULL AND i.due_date < ?
                   AND i.amount_kurus > COALESCE(p.paid_kurus, 0)
             )",
            params![customer_id, today()],
            |row| row.get(0),
        )?;
        object.insert("late_unpaid".to_owned(), json!(i64::from(late)));
    }
    Ok(())
}

fn lowest_available_customer_id(connection: &Connection) -> AppResult<i64> {
    Ok(connection.query_row(
        "SELECT CASE
             WHEN NOT EXISTS(SELECT 1 FROM customers WHERE id = 1) THEN 1
             ELSE COALESCE(
                 (SELECT MIN(current.id + 1)
                  FROM customers current
                  LEFT JOIN customers next ON next.id = current.id + 1
                  WHERE next.id IS NULL),
                 1
             )
         END",
        [],
        |row| row.get(0),
    )?)
}

fn ensure_customer_exists(connection: &Connection, customer_id: i64) -> AppResult<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM customers WHERE id = ?)",
        [customer_id],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(AppError::user("Müşteri bulunamadı."));
    }
    Ok(())
}

fn validate_text_length(label: &str, value: &str, max: usize) -> AppResult<()> {
    if value.chars().count() > max {
        return Err(AppError::user(format!("{label} çok uzun.")));
    }
    Ok(())
}

fn validate_customer_text(
    phone: &str,
    address: &str,
    work_address: &str,
    notes: &str,
) -> AppResult<()> {
    validate_text_length("Telefon", phone, 30)?;
    validate_text_length("Adres", address, 255)?;
    validate_text_length("İş adresi", work_address, 255)?;
    validate_text_length("Notlar", notes, 10_000)
}

fn normalize_contacts(value: &Value) -> AppResult<Vec<ContactInput>> {
    let rows = value
        .as_array()
        .ok_or_else(|| AppError::user("İletişimler bir JSON dizisi olmalıdır."))?;
    rows.iter()
        .map(normalize_contact)
        .filter_map(|result| match result {
            Ok(Some(contact)) => Some(Ok(contact)),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn normalize_contact(value: &Value) -> AppResult<Option<ContactInput>> {
    let mut contact: ContactInput = serde_json::from_value(value.clone())
        .map_err(|_| AppError::user("İletişim kaydı geçersiz."))?;
    contact.name = contact.name.trim().to_owned();
    contact.phone = contact.phone.trim().to_owned();
    contact.home_address = contact.home_address.trim().to_owned();
    contact.work_address = contact.work_address.trim().to_owned();
    validate_contact(&contact)?;
    if contact.name.is_empty()
        && contact.phone.is_empty()
        && contact.home_address.is_empty()
        && contact.work_address.is_empty()
    {
        Ok(None)
    } else {
        Ok(Some(contact))
    }
}

fn validate_contact(contact: &ContactInput) -> AppResult<()> {
    validate_text_length("İletişim adı", &contact.name, 120)?;
    validate_text_length("İletişim telefonu", &contact.phone, 30)?;
    validate_text_length("Ev adresi", &contact.home_address, 255)?;
    validate_text_length("İş adresi", &contact.work_address, 255)
}

fn replace_contacts_tx(
    transaction: &Transaction<'_>,
    customer_id: i64,
    contacts: &[ContactInput],
) -> AppResult<()> {
    transaction.execute("DELETE FROM contacts WHERE customer_id = ?", [customer_id])?;
    for contact in contacts {
        transaction.execute(
            "INSERT INTO contacts(customer_id, name, phone, home_address, work_address) VALUES (?, ?, ?, ?, ?)",
            params![customer_id, contact.name, contact.phone, contact.home_address, contact.work_address],
        )?;
    }
    Ok(())
}

fn contacts_for_customer(connection: &Connection, customer_id: i64) -> AppResult<Vec<Value>> {
    let mut statement = connection.prepare(
        "SELECT id, customer_id, name, phone, home_address, work_address
         FROM contacts WHERE customer_id = ? ORDER BY id",
    )?;
    let rows = statement.query_map([customer_id], contact_row_json)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn contact_json(
    connection: &Connection,
    customer_id: i64,
    contact_id: i64,
) -> AppResult<Option<Value>> {
    Ok(connection
        .query_row(
            "SELECT id, customer_id, name, phone, home_address, work_address
             FROM contacts WHERE customer_id = ? AND id = ?",
            params![customer_id, contact_id],
            contact_row_json,
        )
        .optional()?)
}

fn contact_row_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "customer_id": row.get::<_, i64>(1)?,
        "name": row.get::<_, String>(2)?,
        "phone": row.get::<_, String>(3)?,
        "home_address": row.get::<_, String>(4)?,
        "work_address": row.get::<_, String>(5)?,
    }))
}

fn value_text(object: &Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

impl Database {
    fn list_sales(&self, query: &HashMap<String, String>) -> AppResult<Value> {
        let connection = self.connect()?;
        let customer_id = query
            .get("customer_id")
            .and_then(|value| value.parse::<i64>().ok());
        let include_installments = query_flag(query, "installments");
        let mut rows = sale_bases(&connection)?;
        rows.retain(|sale| {
            let id_matches =
                customer_id.is_none_or(|expected| sale["customer_id"].as_i64() == Some(expected));
            let date = sale["date"].as_str().unwrap_or_default();
            id_matches && date_in_range(date, query)
        });
        if include_installments {
            for sale in &mut rows {
                enrich_sale(&connection, sale)?;
            }
        }
        Ok(Value::Array(rows))
    }

    fn get_sale(&self, id: i64) -> AppResult<Value> {
        let connection = self.connect()?;
        let mut sale =
            sale_base(&connection, id)?.ok_or_else(|| AppError::user("Satış bulunamadı."))?;
        enrich_sale(&connection, &mut sale)?;
        Ok(sale)
    }

    fn create_sale(&self, body: &Value) -> AppResult<Value> {
        let body = object(body)?;
        let customer_id = i64_field(body, "customer_id")?
            .filter(|id| *id > 0)
            .ok_or_else(|| AppError::user("Müşteri numarası zorunludur."))?;
        let date = require_date(text_field(body, "date"), true, "Satış tarihi")?;
        let total_kurus = body
            .get("total")
            .ok_or_else(|| AppError::user("Satış tutarı zorunludur."))
            .and_then(money_to_kurus)?;
        if total_kurus <= 0 {
            return Err(AppError::user("Satış tutarı sıfırdan büyük olmalıdır."));
        }
        let description = text_field(body, "description").unwrap_or_default();
        validate_text_length("Satış açıklaması", &description, 10_000)?;
        let request_key = text_field(body, "request_key")
            .map(|value| sanitize_request_key(&value))
            .filter(|value| !value.is_empty());
        let bundled = body.contains_key("installments");
        let installments = prepare_sale_installments(body, total_kurus)?;

        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        ensure_customer_exists(&transaction, customer_id)?;

        if let Some(key) = request_key.as_deref() {
            if let Some(existing_id) = transaction
                .query_row("SELECT id FROM sales WHERE request_key = ?", [key], |row| {
                    row.get::<_, i64>(0)
                })
                .optional()?
            {
                return sale_create_response(&transaction, existing_id, bundled);
            }
        }

        transaction.execute(
            "INSERT INTO sales(customer_id, date, total_kurus, description, request_key) VALUES (?, ?, ?, ?, ?)",
            params![customer_id, date, total_kurus, description, request_key],
        )?;
        let sale_id = transaction.last_insert_rowid();
        for installment in installments {
            insert_installment_tx(&transaction, sale_id, &installment)?;
        }
        mark_modified(&transaction)?;
        let response = sale_create_response(&transaction, sale_id, bundled)?;
        transaction.commit()?;
        Ok(response)
    }

    fn update_sale(&self, id: i64, body: &Value) -> AppResult<Value> {
        let body = object(body)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let current: Option<(i64, String, i64, String)> = transaction
            .query_row(
                "SELECT customer_id, date, total_kurus, description FROM sales WHERE id = ?",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(AppError::user("Satış bulunamadı."));
        };

        let customer_id = i64_field(body, "customer_id")?.unwrap_or(current.0);
        ensure_customer_exists(&transaction, customer_id)?;
        let date = text_field(body, "date").unwrap_or(current.1);
        validate_iso_date(&date, "Satış tarihi")?;
        let total_kurus = body
            .get("total")
            .map(money_to_kurus)
            .transpose()?
            .unwrap_or(current.2);
        if total_kurus <= 0 {
            return Err(AppError::user("Satış tutarı sıfırdan büyük olmalıdır."));
        }
        let installment_total: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(amount_kurus), 0) FROM installments WHERE sale_id = ?",
            [id],
            |row| row.get(0),
        )?;
        if total_kurus < installment_total {
            return Err(AppError::user(
                "Satış tutarı taksitlerin toplamından küçük olamaz.",
            ));
        }
        let description = text_field(body, "description").unwrap_or(current.3);
        validate_text_length("Satış açıklaması", &description, 10_000)?;

        transaction.execute(
            "UPDATE sales SET customer_id = ?, date = ?, total_kurus = ?, description = ? WHERE id = ?",
            params![customer_id, date, total_kurus, description, id],
        )?;
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "updated": true }))
    }

    fn delete_sale(&self, id: i64) -> AppResult<Value> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        if transaction.execute("DELETE FROM sales WHERE id = ?", [id])? == 0 {
            return Err(AppError::user("Satış bulunamadı."));
        }
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "deleted": true }))
    }
}

fn sale_bases(connection: &Connection) -> AppResult<Vec<Value>> {
    let mut statement = connection.prepare(
        "SELECT s.id, s.customer_id, s.date, s.total_kurus, s.description, s.request_key,
                COALESCE(c.name, '')
         FROM sales s LEFT JOIN customers c ON c.id = s.customer_id
         ORDER BY s.date DESC, s.id DESC",
    )?;
    let rows = statement.query_map([], sale_row_json)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn sale_base(connection: &Connection, id: i64) -> AppResult<Option<Value>> {
    Ok(connection
        .query_row(
            "SELECT s.id, s.customer_id, s.date, s.total_kurus, s.description, s.request_key,
                    COALESCE(c.name, '')
             FROM sales s LEFT JOIN customers c ON c.id = s.customer_id WHERE s.id = ?",
            [id],
            sale_row_json,
        )
        .optional()?)
}

fn sale_row_json(row: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let total: i64 = row.get(3)?;
    Ok(json!({
        "id": row.get::<_, i64>(0)?,
        "customer_id": row.get::<_, i64>(1)?,
        "date": row.get::<_, String>(2)?,
        "total": money_json(total),
        "description": row.get::<_, String>(4)?,
        "request_key": row.get::<_, Option<String>>(5)?,
        "customer_name": row.get::<_, String>(6)?,
    }))
}

fn enrich_sale(connection: &Connection, sale: &mut Value) -> AppResult<()> {
    let sale_id = sale["id"]
        .as_i64()
        .ok_or_else(|| AppError::user("Satış numarası okunamadı."))?;
    let installments = installments_for_sale(connection, sale_id, true)?;
    let installments_total = sum_json_money(&installments, "amount")?;
    let paid_total = sum_json_money(&installments, "paid_amount")?;
    let remaining_total = sum_json_money(&installments, "remaining_amount")?;
    let object = sale
        .as_object_mut()
        .ok_or_else(|| AppError::user("Satış kaydı geçersiz."))?;
    object.insert("installments".to_owned(), Value::Array(installments));
    object.insert(
        "installments_total".to_owned(),
        money_json(installments_total),
    );
    object.insert("installments_paid_total".to_owned(), money_json(paid_total));
    object.insert(
        "installments_remaining_total".to_owned(),
        money_json(remaining_total),
    );
    Ok(())
}

fn sum_json_money(rows: &[Value], key: &str) -> AppResult<i64> {
    rows.iter().try_fold(0_i64, |sum, row| {
        let amount = row
            .get(key)
            .ok_or_else(|| AppError::user("Para toplamı okunamadı."))
            .and_then(money_to_kurus)?;
        sum.checked_add(amount)
            .ok_or_else(|| AppError::user("Para toplamı desteklenen aralığı aşıyor."))
    })
}

fn sanitize_request_key(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || "_.:-".contains(*character))
        .take(64)
        .collect()
}

fn prepare_sale_installments(
    body: &Map<String, Value>,
    total_kurus: i64,
) -> AppResult<Vec<NewInstallment>> {
    let Some(value) = body.get("installments") else {
        return Ok(Vec::new());
    };
    let rows = value
        .as_array()
        .ok_or_else(|| AppError::user("Taksitler bir JSON dizisi olmalıdır."))?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let explicit_down = body.get("down_payment").map(money_to_kurus).transpose()?;
    if explicit_down.is_some_and(|down| down < 0 || down > total_kurus) {
        return Err(AppError::user("Peşinat satış tutarı aralığında olmalıdır."));
    }
    let all_amounts_supplied = rows.iter().all(|row| row.get("amount").is_some());
    let supplied_total = if all_amounts_supplied {
        rows.iter().try_fold(0_i64, |sum, row| {
            let amount = money_to_kurus(&row["amount"])?;
            sum.checked_add(amount)
                .ok_or_else(|| AppError::user("Taksit toplamı desteklenen aralığı aşıyor."))
        })?
    } else {
        total_kurus
    };
    let target = explicit_down
        .map(|down| total_kurus - down)
        .unwrap_or(supplied_total);
    if target <= 0 {
        return Err(AppError::user("Taksit toplamı sıfırdan büyük olmalıdır."));
    }

    let count = i64::try_from(rows.len()).map_err(|_| AppError::user("Çok fazla taksit var."))?;
    let even_amount = target / count;
    let mut result = Vec::with_capacity(rows.len());
    let mut allocated = 0_i64;
    for (index, value) in rows.iter().enumerate() {
        let row = object(value)?;
        let amount_kurus = if index + 1 == rows.len() {
            target
                .checked_sub(allocated)
                .ok_or_else(|| AppError::user("Taksit toplamı satış bakiyesini aşıyor."))?
        } else {
            row.get("amount")
                .map(money_to_kurus)
                .transpose()?
                .unwrap_or(even_amount)
        };
        if amount_kurus <= 0 {
            return Err(AppError::user("Her taksit sıfırdan büyük olmalıdır."));
        }
        allocated = allocated
            .checked_add(amount_kurus)
            .ok_or_else(|| AppError::user("Taksit toplamı desteklenen aralığı aşıyor."))?;
        if allocated > target {
            return Err(AppError::user("Taksit toplamı satış bakiyesini aşıyor."));
        }
        let due_date = text_field(row, "due_date").filter(|date| !date.is_empty());
        if let Some(date) = due_date.as_deref() {
            validate_iso_date(date, "Taksit vadesi")?;
        }
        let payment_date = text_field(row, "payment_date").filter(|date| !date.is_empty());
        if let Some(date) = payment_date.as_deref() {
            validate_iso_date(date, "Ödeme tarihi")?;
        }
        result.push(NewInstallment {
            due_date,
            amount_kurus,
            paid: bool_field(row, "paid").unwrap_or(false),
            payment_date,
        });
    }
    debug_assert_eq!(allocated, target);
    Ok(result)
}

fn sale_create_response(
    connection: &Connection,
    sale_id: i64,
    include_installments: bool,
) -> AppResult<Value> {
    if !include_installments {
        return Ok(json!({ "id": sale_id }));
    }
    let mut statement = connection
        .prepare("SELECT id FROM installments WHERE sale_id = ? ORDER BY due_date, id")?;
    let ids = statement
        .query_map([sale_id], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(json!({ "id": sale_id, "installment_ids": ids }))
}

impl Database {
    fn list_installments(&self, query: &HashMap<String, String>) -> AppResult<Value> {
        let connection = self.connect()?;
        let sale_id = query
            .get("sale_id")
            .and_then(|value| value.parse::<i64>().ok());
        let ids = installment_ids(&connection, sale_id)?;
        let rows = ids
            .into_iter()
            .map(|id| installment_json(&connection, id, true))
            .collect::<AppResult<Vec<_>>>()?;
        Ok(Value::Array(rows))
    }

    fn get_installment(&self, id: i64) -> AppResult<Value> {
        let connection = self.connect()?;
        installment_json(&connection, id, true)
    }

    fn create_installment(&self, body: &Value) -> AppResult<Value> {
        let body = object(body)?;
        let sale_id = i64_field(body, "sale_id")?
            .filter(|id| *id > 0)
            .ok_or_else(|| AppError::user("Satış numarası zorunludur."))?;
        let amount_kurus = body
            .get("amount")
            .ok_or_else(|| AppError::user("Taksit tutarı zorunludur."))
            .and_then(money_to_kurus)?;
        if amount_kurus <= 0 {
            return Err(AppError::user("Taksit tutarı sıfırdan büyük olmalıdır."));
        }
        let due_date = text_field(body, "due_date").filter(|value| !value.is_empty());
        if let Some(date) = due_date.as_deref() {
            validate_iso_date(date, "Taksit vadesi")?;
        }
        let payment_date = text_field(body, "payment_date").filter(|value| !value.is_empty());
        if let Some(date) = payment_date.as_deref() {
            validate_iso_date(date, "Ödeme tarihi")?;
        }
        let installment = NewInstallment {
            due_date,
            amount_kurus,
            paid: bool_field(body, "paid").unwrap_or(false),
            payment_date,
        };

        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        ensure_sale_exists(&transaction, sale_id)?;
        let id = insert_installment_tx(&transaction, sale_id, &installment)?;
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "id": id }))
    }

    fn update_installment(&self, id: i64, body: &Value) -> AppResult<Value> {
        let body = object(body)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let current: Option<(Option<String>, i64)> = transaction
            .query_row(
                "SELECT due_date, amount_kurus FROM installments WHERE id = ?",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(AppError::user("Taksit bulunamadı."));
        };
        if !body.contains_key("due_date")
            && !body.contains_key("amount")
            && !body.contains_key("paid")
        {
            return Err(AppError::user("Güncellenecek alan bulunamadı."));
        }

        let due_date = if body.contains_key("due_date") {
            text_field(body, "due_date").filter(|value| !value.is_empty())
        } else {
            current.0
        };
        if let Some(date) = due_date.as_deref() {
            validate_iso_date(date, "Taksit vadesi")?;
        }
        let amount_kurus = body
            .get("amount")
            .map(money_to_kurus)
            .transpose()?
            .unwrap_or(current.1);
        if amount_kurus <= 0 {
            return Err(AppError::user("Taksit tutarı sıfırdan büyük olmalıdır."));
        }
        let paid_kurus = installment_paid_kurus(&transaction, id)?;
        if amount_kurus < paid_kurus {
            return Err(AppError::user(
                "Taksit tutarı alınmış ödemelerin toplamından küçük olamaz.",
            ));
        }

        transaction.execute(
            "UPDATE installments SET due_date = ?, amount_kurus = ? WHERE id = ?",
            params![due_date, amount_kurus, id],
        )?;
        if let Some(paid) = bool_field(body, "paid") {
            if paid {
                let remaining = amount_kurus - paid_kurus;
                if remaining > 0 {
                    let payment_date =
                        require_date(text_field(body, "payment_date"), true, "Ödeme tarihi")?;
                    insert_payment_raw_tx(&transaction, id, remaining, &payment_date)?;
                }
            } else {
                transaction.execute(
                    "DELETE FROM installment_payments WHERE installment_id = ?",
                    [id],
                )?;
            }
        }
        recalculate_paid_date(&transaction, id)?;
        mark_modified(&transaction)?;
        let installment = installment_json(&transaction, id, true)?;
        transaction.commit()?;
        Ok(json!({ "updated": true, "installment": installment }))
    }

    fn delete_installment(&self, id: i64) -> AppResult<Value> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        if transaction.execute("DELETE FROM installments WHERE id = ?", [id])? == 0 {
            return Err(AppError::user("Taksit bulunamadı."));
        }
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "deleted": true }))
    }

    fn list_payments(&self, installment_id: i64) -> AppResult<Value> {
        let connection = self.connect()?;
        let installment = installment_json(&connection, installment_id, true)?;
        let payments = installment["payments"].clone();
        Ok(json!({ "installment": installment, "payments": payments }))
    }

    fn get_payment(&self, installment_id: i64, payment_id: i64) -> AppResult<Value> {
        let connection = self.connect()?;
        payment_json(&connection, installment_id, payment_id)?
            .ok_or_else(|| AppError::user("Ödeme kaydı bulunamadı."))
    }

    fn create_payment(&self, installment_id: i64, body: &Value) -> AppResult<Value> {
        let body = object(body)?;
        let amount = body.get("amount").map(money_to_kurus).transpose()?;
        let payment_date = require_date(text_field(body, "payment_date"), true, "Ödeme tarihi")?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let response = create_payment_tx(&transaction, installment_id, amount, &payment_date)?;
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(response)
    }

    fn update_payment(
        &self,
        installment_id: i64,
        payment_id: i64,
        body: &Value,
    ) -> AppResult<Value> {
        let body = object(body)?;
        if !body.contains_key("amount") && !body.contains_key("payment_date") {
            return Err(AppError::user("Güncellenecek alan bulunamadı."));
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        let current: Option<(i64, String)> = transaction
            .query_row(
                "SELECT amount_kurus, payment_date FROM installment_payments WHERE id = ? AND installment_id = ?",
                params![payment_id, installment_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some(current) = current else {
            return Err(AppError::user("Ödeme kaydı bulunamadı."));
        };
        let amount = body
            .get("amount")
            .map(money_to_kurus)
            .transpose()?
            .unwrap_or(current.0);
        if amount <= 0 {
            return Err(AppError::user("Ödeme tutarı sıfırdan büyük olmalıdır."));
        }
        let payment_date = text_field(body, "payment_date").unwrap_or(current.1);
        validate_iso_date(&payment_date, "Ödeme tarihi")?;
        let installment_amount: i64 = transaction
            .query_row(
                "SELECT amount_kurus FROM installments WHERE id = ?",
                [installment_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| AppError::user("Taksit bulunamadı."))?;
        let other_paid: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(amount_kurus), 0) FROM installment_payments WHERE installment_id = ? AND id <> ?",
            params![installment_id, payment_id],
            |row| row.get(0),
        )?;
        if other_paid
            .checked_add(amount)
            .is_none_or(|total| total > installment_amount)
        {
            return Err(AppError::user("Ödeme tutarı kalan borçtan büyük olamaz."));
        }
        transaction.execute(
            "UPDATE installment_payments SET amount_kurus = ?, payment_date = ? WHERE id = ?",
            params![amount, payment_date, payment_id],
        )?;
        recalculate_paid_date(&transaction, installment_id)?;
        mark_modified(&transaction)?;
        let payment = payment_json(&transaction, installment_id, payment_id)?
            .ok_or_else(|| AppError::user("Ödeme kaydı bulunamadı."))?;
        let installment = installment_json(&transaction, installment_id, true)?;
        transaction.commit()?;
        Ok(json!({
            "updated": true,
            "payment": payment,
            "installment": installment,
        }))
    }

    fn delete_payment(&self, installment_id: i64, payment_id: i64) -> AppResult<Value> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        if transaction.execute(
            "DELETE FROM installment_payments WHERE id = ? AND installment_id = ?",
            params![payment_id, installment_id],
        )? == 0
        {
            return Err(AppError::user("Ödeme kaydı bulunamadı."));
        }
        recalculate_paid_date(&transaction, installment_id)?;
        mark_modified(&transaction)?;
        let installment = installment_json(&transaction, installment_id, true)?;
        transaction.commit()?;
        Ok(json!({ "deleted": true, "installment": installment }))
    }
}

fn ensure_sale_exists(connection: &Connection, sale_id: i64) -> AppResult<()> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sales WHERE id = ?)",
        [sale_id],
        |row| row.get(0),
    )?;
    if !exists {
        return Err(AppError::user("Satış bulunamadı."));
    }
    Ok(())
}

fn insert_installment_tx(
    transaction: &Transaction<'_>,
    sale_id: i64,
    installment: &NewInstallment,
) -> AppResult<i64> {
    transaction.execute(
        "INSERT INTO installments(sale_id, due_date, amount_kurus, paid_date) VALUES (?, ?, ?, NULL)",
        params![sale_id, installment.due_date, installment.amount_kurus],
    )?;
    let id = transaction.last_insert_rowid();
    if installment.paid {
        let payment_date = installment.payment_date.clone().unwrap_or_else(today);
        validate_iso_date(&payment_date, "Ödeme tarihi")?;
        insert_payment_raw_tx(transaction, id, installment.amount_kurus, &payment_date)?;
    }
    recalculate_paid_date(transaction, id)?;
    Ok(id)
}

fn installment_ids(connection: &Connection, sale_id: Option<i64>) -> AppResult<Vec<i64>> {
    let (sql, parameter): (&str, Option<i64>) = if sale_id.is_some() {
        (
            "SELECT id FROM installments WHERE sale_id = ? ORDER BY due_date, id",
            sale_id,
        )
    } else {
        ("SELECT id FROM installments ORDER BY due_date, id", None)
    };
    let mut statement = connection.prepare(sql)?;
    if let Some(sale_id) = parameter {
        let rows = statement.query_map([sale_id], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    } else {
        let rows = statement.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn installments_for_sale(
    connection: &Connection,
    sale_id: i64,
    include_payments: bool,
) -> AppResult<Vec<Value>> {
    installment_ids(connection, Some(sale_id))?
        .into_iter()
        .map(|id| installment_json(connection, id, include_payments))
        .collect()
}

fn installment_json(
    connection: &Connection,
    installment_id: i64,
    include_payments: bool,
) -> AppResult<Value> {
    let row: Option<InstallmentDbRow> = connection
        .query_row(
            "SELECT id, sale_id, due_date, amount_kurus, paid_date FROM installments WHERE id = ?",
            [installment_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((id, sale_id, due_date, amount, stored_paid_date)) = row else {
        return Err(AppError::user("Taksit bulunamadı."));
    };

    let payments = payment_rows(connection, id, amount)?;
    let paid_amount = installment_paid_kurus(connection, id)?;
    let remaining = amount.saturating_sub(paid_amount).max(0);
    let paid = remaining == 0;
    let last = payments.last();
    let last_payment_id = last.and_then(|row| row["id"].as_i64());
    let last_payment_amount = last
        .and_then(|row| row.get("amount"))
        .cloned()
        .unwrap_or_else(|| money_json(0));
    let last_payment_date = last
        .and_then(|row| row["payment_date"].as_str())
        .map(str::to_owned);
    let paid_date = if paid {
        stored_paid_date.or_else(|| last_payment_date.clone())
    } else {
        None
    };

    let mut result = json!({
        "id": id,
        "sale_id": sale_id,
        "due_date": due_date,
        "amount": money_json(amount),
        "paid": i64::from(paid),
        "paid_date": paid_date,
        "paid_amount": money_json(paid_amount),
        "remaining_amount": money_json(remaining),
        "payment_count": payments.len(),
        "last_payment_id": last_payment_id,
        "last_payment_amount": last_payment_amount,
        "last_payment_date": last_payment_date,
    });
    if include_payments {
        result["payments"] = Value::Array(payments);
    }
    Ok(result)
}

fn payment_rows(
    connection: &Connection,
    installment_id: i64,
    installment_amount: i64,
) -> AppResult<Vec<Value>> {
    let mut statement = connection.prepare(
        "SELECT id, installment_id, amount_kurus, payment_date, created_at
         FROM installment_payments WHERE installment_id = ? ORDER BY payment_date, id",
    )?;
    let mut running = 0_i64;
    let rows = statement.query_map([installment_id], |row| {
        let amount: i64 = row.get(2)?;
        running = running.saturating_add(amount);
        Ok(json!({
            "id": row.get::<_, i64>(0)?,
            "installment_id": row.get::<_, i64>(1)?,
            "amount": money_json(amount),
            "payment_date": row.get::<_, String>(3)?,
            "created_at": row.get::<_, String>(4)?,
            "running_paid_amount": money_json(running),
            "remaining_after_payment": money_json(installment_amount.saturating_sub(running).max(0)),
        }))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

fn installment_paid_kurus(connection: &Connection, installment_id: i64) -> AppResult<i64> {
    Ok(connection.query_row(
        "SELECT COALESCE(SUM(amount_kurus), 0) FROM installment_payments WHERE installment_id = ?",
        [installment_id],
        |row| row.get(0),
    )?)
}

fn insert_payment_raw_tx(
    transaction: &Transaction<'_>,
    installment_id: i64,
    amount: i64,
    payment_date: &str,
) -> AppResult<i64> {
    transaction.execute(
        "INSERT INTO installment_payments(installment_id, amount_kurus, payment_date, created_at) VALUES (?, ?, ?, ?)",
        params![installment_id, amount, payment_date, created_at()],
    )?;
    Ok(transaction.last_insert_rowid())
}

fn create_payment_tx(
    transaction: &Transaction<'_>,
    installment_id: i64,
    requested_amount: Option<i64>,
    payment_date: &str,
) -> AppResult<Value> {
    let installment_amount: i64 = transaction
        .query_row(
            "SELECT amount_kurus FROM installments WHERE id = ?",
            [installment_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| AppError::user("Taksit bulunamadı."))?;
    let paid_before = installment_paid_kurus(transaction, installment_id)?;
    let remaining = installment_amount.saturating_sub(paid_before);
    if remaining <= 0 {
        return Err(AppError::user("Bu taksit tamamen ödenmiş."));
    }
    let amount = requested_amount.unwrap_or(remaining);
    if amount <= 0 {
        return Err(AppError::user("Ödeme tutarı sıfırdan büyük olmalıdır."));
    }
    if amount > remaining {
        return Err(AppError::user("Ödeme tutarı kalan borçtan büyük olamaz."));
    }
    let payment_id = insert_payment_raw_tx(transaction, installment_id, amount, payment_date)?;
    recalculate_paid_date(transaction, installment_id)?;
    let paid_after = paid_before + amount;
    let installment = installment_json(transaction, installment_id, false)?;
    Ok(json!({
        "payment": {
            "id": payment_id,
            "installment_id": installment_id,
            "amount": money_json(amount),
            "payment_date": payment_date,
            "paid_before": money_json(paid_before),
            "paid_after": money_json(paid_after),
            "remaining_after_payment": money_json(installment_amount - paid_after),
        },
        "installment": installment,
    }))
}

fn recalculate_paid_date(connection: &Connection, installment_id: i64) -> AppResult<()> {
    let (amount, paid): (i64, i64) = connection
        .query_row(
            "SELECT i.amount_kurus, COALESCE(SUM(p.amount_kurus), 0)
             FROM installments i LEFT JOIN installment_payments p ON p.installment_id = i.id
             WHERE i.id = ? GROUP BY i.id",
            [installment_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| AppError::user("Taksit bulunamadı."))?;
    let paid_date: Option<String> = if paid >= amount {
        connection
            .query_row(
                "SELECT payment_date FROM installment_payments WHERE installment_id = ? ORDER BY payment_date DESC, id DESC LIMIT 1",
                [installment_id],
                |row| row.get(0),
            )
            .optional()?
    } else {
        None
    };
    connection.execute(
        "UPDATE installments SET paid_date = ? WHERE id = ?",
        params![paid_date, installment_id],
    )?;
    Ok(())
}

fn payment_json(
    connection: &Connection,
    installment_id: i64,
    payment_id: i64,
) -> AppResult<Option<Value>> {
    let installment_amount: Option<i64> = connection
        .query_row(
            "SELECT amount_kurus FROM installments WHERE id = ?",
            [installment_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(installment_amount) = installment_amount else {
        return Ok(None);
    };
    Ok(
        payment_rows(connection, installment_id, installment_amount)?
            .into_iter()
            .find(|payment| payment["id"].as_i64() == Some(payment_id)),
    )
}

impl Database {
    fn expected_payments(&self, query: &HashMap<String, String>) -> AppResult<Value> {
        let connection = self.connect()?;
        Ok(Value::Array(expected_payment_rows(&connection, query)?))
    }

    fn daily_report(&self, query: &HashMap<String, String>) -> AppResult<Value> {
        let connection = self.connect()?;
        let mut rows = Vec::new();

        let mut sale_statement = connection.prepare(
            "SELECT s.id, s.customer_id, s.date, s.total_kurus, s.description,
                    COALESCE(c.name, ''), COUNT(i.id), COALESCE(SUM(i.amount_kurus), 0)
             FROM sales s
             LEFT JOIN customers c ON c.id = s.customer_id
             LEFT JOIN installments i ON i.sale_id = s.id
             GROUP BY s.id
             ORDER BY s.date DESC, s.id DESC",
        )?;
        let sale_rows = sale_statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })?;
        for row in sale_rows {
            let (
                sale_id,
                customer_id,
                date,
                sale_total,
                description,
                customer_name,
                count,
                installments_total,
            ) = row?;
            if !date_in_range(&date, query) {
                continue;
            }
            let amount = if count > 0 {
                sale_total.saturating_sub(installments_total).max(0)
            } else {
                sale_total
            };
            if amount <= 0 {
                continue;
            }
            rows.push(json!({
                "event_id": format!("sale-{sale_id}"),
                "event_type": if count > 0 { "down_payment" } else { "sale" },
                "id": sale_id,
                "sale_id": sale_id,
                "payment_id": Value::Null,
                "installment_id": Value::Null,
                "customer_id": customer_id,
                "customer_name": customer_name,
                "date": date,
                "total": money_json(amount),
                "amount": money_json(amount),
                "sale_total": money_json(sale_total),
                "description": description,
                "_sort_id": sale_id,
            }));
        }
        drop(sale_statement);

        let mut payment_statement = connection.prepare(
            "SELECT p.id, p.installment_id, p.amount_kurus, p.payment_date,
                    i.sale_id, s.customer_id, s.total_kurus, s.description,
                    COALESCE(c.name, '')
             FROM installment_payments p
             JOIN installments i ON i.id = p.installment_id
             JOIN sales s ON s.id = i.sale_id
             LEFT JOIN customers c ON c.id = s.customer_id
             ORDER BY p.payment_date DESC, p.id DESC",
        )?;
        let payment_rows = payment_statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
            ))
        })?;
        for row in payment_rows {
            let (
                payment_id,
                installment_id,
                amount,
                date,
                sale_id,
                customer_id,
                sale_total,
                description,
                customer_name,
            ) = row?;
            if amount <= 0 || !date_in_range(&date, query) {
                continue;
            }
            rows.push(json!({
                "event_id": format!("payment-{payment_id}"),
                "event_type": "installment_payment",
                "id": sale_id,
                "sale_id": sale_id,
                "payment_id": payment_id,
                "installment_id": installment_id,
                "customer_id": customer_id,
                "customer_name": customer_name,
                "date": date,
                "total": money_json(amount),
                "amount": money_json(amount),
                "sale_total": money_json(sale_total),
                "description": description,
                "_sort_id": payment_id,
            }));
        }

        rows.sort_by(|left, right| {
            let date_order = right["date"]
                .as_str()
                .unwrap_or_default()
                .cmp(left["date"].as_str().unwrap_or_default());
            if date_order == Ordering::Equal {
                right["_sort_id"]
                    .as_i64()
                    .unwrap_or_default()
                    .cmp(&left["_sort_id"].as_i64().unwrap_or_default())
            } else {
                date_order
            }
        });
        for row in &mut rows {
            row.as_object_mut()
                .expect("report row must be an object")
                .remove("_sort_id");
        }
        Ok(Value::Array(rows))
    }

    fn get_business_profile_json(&self) -> AppResult<Value> {
        Ok(serde_json::to_value(self.business_profile()?)?)
    }

    fn update_business_profile(&self, body: &Value) -> AppResult<Value> {
        let mut profile: BusinessProfile = serde_json::from_value(body.clone())
            .map_err(|_| AppError::user("İşletme bilgileri geçersiz."))?;
        profile.name = profile.name.trim().to_owned();
        profile.address = profile.address.trim().to_owned();
        profile.phone = profile.phone.trim().to_owned();
        profile.website = profile.website.trim().to_owned();
        profile.footer_sub = profile.footer_sub.trim().to_owned();
        validate_profile(&profile)?;

        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        write_business_profile(&transaction, &profile)?;
        mark_modified(&transaction)?;
        transaction.commit()?;
        Ok(json!({ "updated": true, "business": profile }))
    }

    fn offline_snapshot(&self) -> AppResult<Value> {
        let connection = self.connect()?;
        let mut customers = customer_bases(&connection)?;
        for customer in &mut customers {
            enrich_customer(&connection, customer, true, true)?;
        }
        let mut sales = sale_bases(&connection)?;
        for sale in &mut sales {
            enrich_sale(&connection, sale)?;
        }
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "synced_at": Local::now().to_rfc3339(),
            "business": read_business_profile(&connection)?,
            "customers": customers,
            "sales": sales,
            "expected_payments": expected_payment_rows(&connection, &HashMap::new())?,
        }))
    }
}

fn expected_payment_rows(
    connection: &Connection,
    query: &HashMap<String, String>,
) -> AppResult<Vec<Value>> {
    let mut statement = connection.prepare(
        "SELECT i.id, i.due_date, i.amount_kurus, i.paid_date,
                s.id, s.date, s.total_kurus, s.description,
                c.id, c.name, c.phone, c.address, c.work_address
         FROM installments i
         JOIN sales s ON s.id = i.sale_id
         JOIN customers c ON c.id = s.customer_id
         WHERE i.due_date IS NOT NULL
         ORDER BY i.due_date, i.id",
    )?;
    let base_rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, i64>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, String>(10)?,
            row.get::<_, String>(11)?,
            row.get::<_, String>(12)?,
        ))
    })?;
    let bases = base_rows.collect::<rusqlite::Result<Vec<_>>>()?;
    drop(statement);

    let today = today();
    let mut rows = Vec::new();
    for base in bases {
        let (
            installment_id,
            due_date,
            amount,
            paid_date,
            sale_id,
            sale_date,
            sale_total,
            sale_description,
            customer_id,
            customer_name,
            customer_phone,
            customer_address,
            customer_work_address,
        ) = base;
        if !date_in_range(&due_date, query) {
            continue;
        }
        let installment = installment_json(connection, installment_id, false)?;
        let remaining = money_to_kurus(&installment["remaining_amount"])?;
        let effective_paid_date = installment["paid_date"].as_str().or(paid_date.as_deref());
        if remaining <= 0 && effective_paid_date != Some(today.as_str()) {
            continue;
        }
        rows.push(json!({
            "installment_id": installment_id,
            "due_date": due_date,
            "amount": money_json(amount),
            "installment_amount": money_json(amount),
            "paid": installment["paid"],
            "paid_date": installment["paid_date"],
            "paid_amount": installment["paid_amount"],
            "remaining_amount": installment["remaining_amount"],
            "payment_count": installment["payment_count"],
            "last_payment_id": installment["last_payment_id"],
            "last_payment_amount": installment["last_payment_amount"],
            "last_payment_date": installment["last_payment_date"],
            "sale_id": sale_id,
            "sale_date": sale_date,
            "sale_total": money_json(sale_total),
            "sale_description": sale_description,
            "customer_id": customer_id,
            "customer_name": customer_name,
            "customer_phone": customer_phone,
            "customer_address": customer_address,
            "customer_work_address": customer_work_address,
        }));
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_money_to_integer_kurus_with_rounding() {
        assert_eq!(money_to_kurus(&json!("10.005")).unwrap(), 1_001);
        assert_eq!(money_to_kurus(&json!("10,004")).unwrap(), 1_000);
        assert_eq!(money_to_kurus(&json!(0.1)).unwrap(), 10);
    }

    #[test]
    fn rejects_money_and_ids_outside_javascript_safe_integer_range() {
        assert_eq!(
            money_to_kurus(&json!("90071992547409.91")).unwrap(),
            MAX_SAFE_JS_INTEGER
        );
        assert!(money_to_kurus(&json!("90071992547409.92")).is_err());
        assert_eq!(
            money_to_kurus(&json!("-90071992547409.91")).unwrap(),
            -MAX_SAFE_JS_INTEGER
        );
        assert!(money_to_kurus(&json!("-90071992547409.92")).is_err());
        assert_eq!(
            parse_id(&MAX_SAFE_JS_INTEGER.to_string(), "kayıt").unwrap(),
            MAX_SAFE_JS_INTEGER
        );
        assert!(parse_id(&(MAX_SAFE_JS_INTEGER + 1).to_string(), "kayıt").is_err());
        assert!(i64_field(
            json!({ "customer_id": MAX_SAFE_JS_INTEGER + 1 })
                .as_object()
                .unwrap(),
            "customer_id"
        )
        .is_err());
    }
}
