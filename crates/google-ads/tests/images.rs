//! Images: reference (`ImageRef`), loading from a file and from a URL, dimension
//! checks and the asset creation operation. No test goes out to the network: the URL
//! goes to a local `wiremock` with `AllowHttp::ForTests`.

use adsconsole_google_ads::images::{
    self, AllowHttp, ImageError, ImageKind, ImageRef, Loaded, MAX_IMAGE_BYTES,
};
use base64::Engine as _;
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Minimal PNG: signature + `IHDR` header with the dimensions (enough to detect
/// the format and read the size).
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    b.extend_from_slice(&13u32.to_be_bytes());
    b.extend_from_slice(b"IHDR");
    b.extend_from_slice(&w.to_be_bytes());
    b.extend_from_slice(&h.to_be_bytes());
    b.extend_from_slice(&[8, 6, 0, 0, 0]);
    b.extend_from_slice(&[0, 0, 0, 0]);
    b
}

fn new(l: Loaded) -> images::LoadedImage {
    match l {
        Loaded::New(img) => img,
        Loaded::Existing(rn) => panic!("expected a new image, got {rn}"),
    }
}

#[test]
fn image_checks() {
    assert!(images::check(ImageKind::Landscape, 1200, 628).is_ok());
    assert!(images::check(ImageKind::Landscape, 600, 314).is_ok());
    let e = images::check(ImageKind::Landscape, 1200, 600).unwrap_err();
    assert!(e.contains("1.91:1"), "{e}");
    assert!(images::check(ImageKind::Landscape, 573, 300).is_err()); // aspect ratio ok, too small
    assert!(images::check(ImageKind::Square, 300, 300).is_ok());
    let e = images::check(ImageKind::Square, 200, 200).unwrap_err();
    assert!(e.contains("300x300"), "{e}");
    assert!(images::check(ImageKind::Square, 300, 320).is_err());
    assert!(images::check(ImageKind::Portrait, 480, 600).is_ok());
    assert!(images::check(ImageKind::Portrait, 400, 500).is_err());
    assert!(images::check(ImageKind::Portrait, 600, 480).is_err());
    assert!(images::check(ImageKind::Logo, 128, 128).is_ok());
    assert!(images::check(ImageKind::Logo, 127, 127).is_err());
    assert!(images::check(ImageKind::LandscapeLogo, 512, 128).is_ok());
    assert!(images::check(ImageKind::LandscapeLogo, 512, 256).is_err());
    assert!(images::check(ImageKind::LandscapeLogo, 0, 0).is_err());
}

#[test]
fn image_ref_parsing_is_strict() {
    assert_eq!(
        ImageRef::from_value(&json!({ "file_path": "/tmp/a.png" })).unwrap(),
        ImageRef::File {
            file_path: "/tmp/a.png".into()
        }
    );
    assert_eq!(
        ImageRef::from_value(&json!({ "url": "https://a.com/x.png" })).unwrap(),
        ImageRef::Url {
            url: "https://a.com/x.png".into()
        }
    );
    assert_eq!(
        serde_json::from_value::<ImageRef>(json!({ "asset": "customers/1/assets/2" })).unwrap(),
        ImageRef::Asset {
            asset: "customers/1/assets/2".into()
        }
    );
    for bad in [
        json!("x.png"),
        json!({}),
        json!({ "file_path": "/a.png", "url": "https://a.com/x.png" }),
        json!({ "path": "/a.png" }),
        json!({ "url": 3 }),
        json!({ "asset": "  " }),
    ] {
        assert!(ImageRef::from_value(&bad).is_err(), "{bad}");
        assert!(
            serde_json::from_value::<ImageRef>(bad.clone()).is_err(),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn url_rules() {
    let server = MockServer::start().await;
    let serve = |p: &str, ct: &str, body: Vec<u8>| {
        Mock::given(method("GET"))
            .and(path(p.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, ct))
    };
    serve("/ok.png", "image/png", png(1200, 628))
        .mount(&server)
        .await;
    serve(
        "/page",
        "text/html; charset=utf-8",
        b"<html></html>".to_vec(),
    )
    .mount(&server)
    .await;
    let mut big = png(1200, 628);
    big.resize(MAX_IMAGE_BYTES + 1, 0);
    serve("/big.png", "image/png", big).mount(&server).await;
    serve("/fake.png", "image/png", b"GIF? nope".to_vec())
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/missing.png"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();
    let url = |p: &str| ImageRef::Url {
        url: format!("{}{p}", server.uri()),
    };

    // Production: https only, and local http is refused before any request.
    let e = images::load(&url("/ok.png"), &client).await.unwrap_err();
    assert!(matches!(e, ImageError::NotHttps), "{e:?}");
    assert!(e.to_string().contains("https"), "{e}");
    let e = images::load(
        &ImageRef::Url {
            url: "ftp://a.com/x.png".into(),
        },
        &client,
    )
    .await
    .unwrap_err();
    assert!(matches!(e, ImageError::NotHttps), "{e:?}");
    // Local and private hosts: refused without a request (no DNS either: they are literals).
    for u in [
        "https://localhost/x.png",
        "https://127.0.0.1/x.png",
        "https://10.0.0.1/x.png",
        "https://192.168.1.1/x.png",
        "https://169.254.169.254/latest/meta-data",
        "https://0.0.0.0/x.png",
        "https://[::1]/x.png",
        "https://[fe80::1]/x.png",
        "https://[fd12::1]/x.png",
        "https://[::ffff:127.0.0.1]/x.png",
    ] {
        let r = ImageRef::Url { url: u.into() };
        let e = images::load(&r, images::http_client()).await.unwrap_err();
        assert!(matches!(e, ImageError::ForbiddenHost(_)), "{u}: {e:?}");
        assert!(e.to_string().contains("local or private"), "{e}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());

    let t = AllowHttp::ForTests;
    let e = images::load_with(&client, &url("/page"), t)
        .await
        .unwrap_err();
    assert!(matches!(e, ImageError::NotImage(_)), "{e:?}");
    let e = images::load_with(&client, &url("/big.png"), t)
        .await
        .unwrap_err();
    assert!(matches!(e, ImageError::TooLarge), "{e:?}");
    let e = images::load_with(&client, &url("/fake.png"), t)
        .await
        .unwrap_err();
    assert!(matches!(e, ImageError::UnsupportedFormat), "{e:?}");
    let e = images::load_with(&client, &url("/missing.png"), t)
        .await
        .unwrap_err();
    assert!(matches!(e, ImageError::Http(404)), "{e:?}");

    let img = new(images::load_with(&client, &url("/ok.png"), t)
        .await
        .unwrap());
    assert_eq!((img.width, img.height), (1200, 628));
    assert_eq!(img.mime, "image/png");
    assert_eq!(img.bytes, png(1200, 628));
}

#[tokio::test]
async fn file_rules() {
    let dir = tempfile::tempdir().unwrap();
    let client = reqwest::Client::new();
    let file = |name: &str, bytes: &[u8]| {
        let p = dir.path().join(name);
        std::fs::write(&p, bytes).unwrap();
        ImageRef::File {
            file_path: p.to_string_lossy().into_owned(),
        }
    };

    let img = new(images::load(&file("a.png", &png(300, 300)), &client)
        .await
        .unwrap());
    assert_eq!(
        (img.width, img.height, img.mime.as_str()),
        (300, 300, "image/png")
    );

    let mut jpeg = vec![
        0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00,
    ];
    jpeg.extend_from_slice(&[1, 1, 0, 0, 1, 0, 1, 0, 0]);
    // SOF0 com 628 x 1200.
    jpeg.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08, 0x02, 0x74, 0x04, 0xB0, 0x03]);
    jpeg.extend_from_slice(&[0; 9]);
    let img = new(images::load(&file("a.jpg", &jpeg), &client).await.unwrap());
    assert_eq!(
        (img.width, img.height, img.mime.as_str()),
        (1200, 628, "image/jpeg")
    );

    let mut gif = b"GIF89a".to_vec();
    gif.extend_from_slice(&128u16.to_le_bytes());
    gif.extend_from_slice(&128u16.to_le_bytes());
    gif.extend_from_slice(&[0; 7]);
    let img = new(images::load(&file("a.gif", &gif), &client).await.unwrap());
    assert_eq!(
        (img.width, img.height, img.mime.as_str()),
        (128, 128, "image/gif")
    );

    let e = images::load(&file("x.webp", b"RIFF....WEBPVP8 "), &client)
        .await
        .unwrap_err();
    assert!(matches!(e, ImageError::UnsupportedFormat), "{e:?}");

    // Over 5 MB: refused based on the file size, without reading it all.
    let big = dir.path().join("big.png");
    std::fs::write(&big, png(1200, 628)).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&big)
        .unwrap()
        .set_len(MAX_IMAGE_BYTES as u64 + 1)
        .unwrap();
    let r = ImageRef::File {
        file_path: big.to_string_lossy().into_owned(),
    };
    let e = images::load(&r, &client).await.unwrap_err();
    assert!(matches!(e, ImageError::TooLarge), "{e:?}");

    for bad in ["relative/a.png", "/does/not/exist/a.png"] {
        let r = ImageRef::File {
            file_path: bad.into(),
        };
        let e = images::load(&r, &client).await.unwrap_err();
        assert!(matches!(e, ImageError::File(_)), "{bad}: {e:?}");
    }
    let r = ImageRef::File {
        file_path: dir.path().to_string_lossy().into_owned(),
    };
    assert!(matches!(
        images::load(&r, &client).await.unwrap_err(),
        ImageError::File(_)
    ));

    let r = ImageRef::Asset {
        asset: "customers/1/assets/9".into(),
    };
    assert!(matches!(
        images::load(&r, &client).await.unwrap(),
        Loaded::Existing(rn) if rn == "customers/1/assets/9"
    ));
}

#[test]
fn content_name_comes_from_the_bytes() {
    let img = |bytes: Vec<u8>| images::LoadedImage {
        bytes,
        width: 300,
        height: 300,
        mime: "image/png".into(),
    };
    let a = img(png(300, 300));
    assert_eq!(a.sha256_hex().len(), 64);
    assert_eq!(a.content_name(), format!("img-{}", &a.sha256_hex()[..16]));
    assert_eq!(a.content_name(), img(png(300, 300)).content_name());
    assert_ne!(a.content_name(), img(png(301, 301)).content_name());
}

#[test]
fn image_asset_op_body() {
    let img = images::LoadedImage {
        bytes: png(300, 300),
        width: 300,
        height: 300,
        mime: "image/png".into(),
    };
    let op = images::image_asset_op("customers/1/assets/-3", "Logo", &img);
    assert_eq!(
        op,
        json!({ "assetOperation": { "create": {
            "resourceName": "customers/1/assets/-3",
            "name": "Logo",
            "type": "IMAGE",
            "imageAsset": {
                "data": base64::engine::general_purpose::STANDARD.encode(png(300, 300))
            },
        } } })
    );
}
