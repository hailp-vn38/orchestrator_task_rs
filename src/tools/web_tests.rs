use super::*;

const SAMPLE_HTML: &str = r#"<!DOCTYPE html>
<html lang="vi">
<head>
  <title>Đón không khí lạnh, miền Bắc giảm mấy độ?</title>
  <meta name="description" content="Theo Trung tâm Dự báo khí tượng thủy văn quốc gia, ngày 21 và 22/10 không khí lạnh tràn về." />
  <meta property="og:site_name" content="Tin tức 24h" />
  <meta name="pubdate" content="2026-10-21T18:00:00+07:00" />
  <link rel="canonical" href="https://www.24h.com.vn/tin-tuc-trong-ngay/don-khong-khi-lanh-mien-bac-giam-may-do-c46a1804579.html" />
  <script type="application/ld+json">
  {
    "@context": "http://schema.org",
    "@type": "NewsArticle",
    "headline": "Đón không khí lạnh, miền Bắc giảm mấy độ?",
    "author": { "@type": "Person", "name": "Theo Nam Vũ" }
  }
  </script>
</head>
<body>
  <nav><a href="/home">Trang chủ</a></nav>
  <article class="cate-24h-foot-arti-deta-content-main" id="article_body">
    <header>
      <h1 id="article_title">Đón không khí lạnh, miền Bắc giảm mấy độ?</h1>
    </header>
    <script>var noisy = "should be removed";</script>
    <style>.popup { display: none; }</style>
    <article class="cate-24h-foot-arti-deta-info">
      <h2 id="article_sapo"><strong>Theo Trung tâm Dự báo khí tượng thủy văn quốc gia, ngày 21 và 22/10 không khí lạnh tràn về.</strong></h2>
      <p><img class="news-image" alt="Infographic không khí lạnh" src="data:image/gif;base64,R0lGODlhAQAB" data-original="https://icdn.24h.com.vn/upload/infographic.png" /></p>
      <div class="bv-lq">
        <a href="/tin-tuc-trong-ngay/tac-dong-khong-khi-lanh-c46a1803457.html">Tác động của đợt không khí lạnh sắp tới</a>
        <p>Ngày 21-22/10, khu vực Đông Bắc Bộ và Thanh Hóa có mưa rào &amp; dông rải rác.</p>
      </div>
    </article>
  </article>
</body>
</html>"#;

#[test]
fn entities_and_tags_are_cleaned() {
    assert_eq!(
        strip_tags("<p>Nhiệt độ &lt; 20&#176;C &amp; mưa</p>"),
        "Nhiệt độ < 20°C & mưa"
    );
}

#[test]
fn metadata_is_extracted_from_head_and_json_ld() {
    let meta = extract_metadata(SAMPLE_HTML);
    assert_eq!(meta["title"], "Đón không khí lạnh, miền Bắc giảm mấy độ?");
    assert_eq!(meta["site_name"], "Tin tức 24h");
    assert_eq!(meta["author"], "Theo Nam Vũ");
    assert_eq!(meta["published_at"], "2026-10-21T18:00:00+07:00");
}

#[test]
fn nested_article_strips_noise_and_keeps_content_and_images() {
    let article = extract_article(SAMPLE_HTML);
    assert_eq!(article.title, "Đón không khí lạnh, miền Bắc giảm mấy độ?");
    assert!(article
        .sapo
        .contains("ngày 21 và 22/10 không khí lạnh tràn về"));
    assert_eq!(
        article.sections,
        vec!["Ngày 21-22/10, khu vực Đông Bắc Bộ và Thanh Hóa có mưa rào & dông rải rác."]
    );
    assert_eq!(article.images.len(), 1);
    assert_eq!(
        article.images[0]["url"],
        "https://icdn.24h.com.vn/upload/infographic.png"
    );
}

#[test]
fn links_are_resolved_against_base_url() {
    let links = extract_links(
        SAMPLE_HTML,
        "https://www.24h.com.vn/tin-tuc-trong-ngay/bai-goc.html",
        true,
        10,
    );
    assert_eq!(links.len(), 1);
    assert_eq!(
        links[0]["url"],
        "https://www.24h.com.vn/tin-tuc-trong-ngay/tac-dong-khong-khi-lanh-c46a1803457.html"
    );
}

#[test]
fn feed_items_are_extracted_from_category_listing() {
    let category_html = r#"
    <section>
      <article class="item-news">
        <h3 class="title-news"><a href="https://vnexpress.net/tin-so-1-5001.html">Tin thời sự số 1</a></h3>
        <p class="description"><a href="https://vnexpress.net/tin-so-1-5001.html">Mô tả ngắn tin số 1.</a></p>
      </article>
      <article class="item-news">
        <h3 class="title-news"><a href="/tin-so-2-5002.html">Tin thời sự số 2</a></h3>
        <p class="description">Mô tả ngắn tin số 2.</p>
      </article>
    </section>"#;
    let items = extract_feed_items(category_html, "https://vnexpress.net/thoi-su", 10);
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["title"], "Tin thời sự số 1");
    assert_eq!(items[0]["snippet"], "Mô tả ngắn tin số 1.");
    assert_eq!(items[1]["url"], "https://vnexpress.net/tin-so-2-5002.html");
}
