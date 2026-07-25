/* llmos 站点交互：滚动渐入 + 导航状态 + 当前节高亮
   动效克制（ui-ux-pro-max motion 2/10）：仅 IntersectionObserver 渐入，
   尊重 prefers-reduced-motion（CSS 已兜底）。无依赖，离线可用。 */
(function () {
  "use strict";

  /* 渐进增强：仅在 JS 激活时才应用隐藏初始态（CSS 以 html.js 为门槛），
     无 JS / IO 未触发 / 不滚动的渲染场景下内容默认全部可见。 */
  document.documentElement.classList.add("js");

  var nav = document.getElementById("nav");

  /* 导航滚动态 */
  function onScroll() {
    if (window.scrollY > 24) nav.classList.add("scrolled");
    else nav.classList.remove("scrolled");
  }
  window.addEventListener("scroll", onScroll, { passive: true });
  onScroll();

  /* 滚动渐入 */
  var revealEls = document.querySelectorAll(".reveal, .reveal-group");
  if ("IntersectionObserver" in window) {
    var io = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            entry.target.classList.add("in");
            io.unobserve(entry.target);
          }
        });
      },
      { threshold: 0.12, rootMargin: "0px 0px -8% 0px" }
    );
    revealEls.forEach(function (el) { io.observe(el); });
  } else {
    revealEls.forEach(function (el) { el.classList.add("in"); });
  }

  /* 页内目录当前节高亮（多页站点：跨页高亮由服务端 aria-current 静态标注） */
  var links = Array.prototype.slice.call(
    document.querySelectorAll(".page-toc a")
  );
  var sections = links
    .map(function (a) { return document.querySelector(a.getAttribute("href")); })
    .filter(Boolean);

  if ("IntersectionObserver" in window && sections.length) {
    var byHref = {};
    links.forEach(function (a) { byHref[a.getAttribute("href").slice(1)] = a; });
    var spy = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            links.forEach(function (a) { a.classList.remove("active"); });
            var link = byHref[entry.target.id];
            if (link) link.classList.add("active");
          }
        });
      },
      { rootMargin: "-30% 0px -60% 0px" }
    );
    sections.forEach(function (s) { spy.observe(s); });
  }
})();
