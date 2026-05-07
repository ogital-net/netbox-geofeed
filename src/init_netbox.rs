use crate::cli::InitNetboxArgs;
use futures_util::TryStreamExt as _;
use netbox_client::{CustomFieldRequest, NetboxClient, extras::CustomFieldFilter};

struct FieldSpec {
    name: &'static str,
    label: &'static str,
    description: &'static str,
}

const FIELDS: &[FieldSpec] = &[
    FieldSpec {
        name: "geofeed_country",
        label: "Geofeed Country",
        description: "ISO 3166-1 alpha-2 country code for geofeed generation (e.g. US, DE)",
    },
    FieldSpec {
        name: "geofeed_region",
        label: "Geofeed Region",
        description: "ISO 3166-2 subdivision code for geofeed generation (e.g. CA, NY)",
    },
    FieldSpec {
        name: "geofeed_city",
        label: "Geofeed City",
        description: "City name for geofeed generation",
    },
];

pub async fn run(args: InitNetboxArgs) -> anyhow::Result<()> {
    let client = NetboxClient::new(&args.global.netbox_url, &args.global.netbox_token)?;

    for spec in FIELDS {
        let filter = CustomFieldFilter {
            name: vec![spec.name.to_owned()],
            object_type: Some("dcim.site".to_owned()),
            ..Default::default()
        };

        let existing: Vec<_> = client.custom_fields(&filter).try_collect().await?;

        if !existing.is_empty() {
            log::info!("custom field {} already exists, skipping", spec.name);
            continue;
        }

        let body = CustomFieldRequest {
            object_types: vec!["dcim.site".to_owned()],
            r#type: "text".to_owned(),
            name: spec.name.to_owned(),
            label: Some(spec.label.to_owned()),
            description: Some(spec.description.to_owned()),
            ..Default::default()
        };

        if args.no_write {
            println!(
                "would create custom field: name={}, label={}, description={}",
                spec.name, spec.label, spec.description
            );
        } else {
            let cf = client.custom_field_create(&body).await?;
            log::info!("created custom field {} (id={})", spec.name, cf.id);
        }
    }

    Ok(())
}
