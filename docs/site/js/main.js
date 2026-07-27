/* ============================================================
   llmos 站点交互 v3
   动效克制但精致：滚动渐入 + 视差 + 数字递增 + 流场联动
   - 尊重 prefers-reduced-motion（全量兜底，无 JS 内容全可见）
   - 无依赖，离线可用
   ============================================================ */
(function () {
  "use strict";

  var REDUCE = window.matchMedia &&
    window.matchMedia("(prefers-reduced-motion: reduce)").matches;

  /* 渐进增强：仅在 JS 激活时才隐藏初始态 */
  document.documentElement.classList.add("js");

  var nav = document.getElementById("nav");

  /* ---- 1. 噪点纹理层（叠加在 WebGL 背景之上，消除色块塑料感）---- */
  /* v4：光雾背景已由 Three.js WebGL 流体背景接管（见 effects.js），
     这里仅保留噪点叠加层 */
  function injectNoise() {
    if (document.querySelector(".bg-noise")) return;
    var n = document.createElement("div");
    n.className = "bg-noise";
    n.setAttribute("aria-hidden", "true");
    document.body.insertBefore(n, document.body.firstChild);
  }
  injectNoise();

  /* ---- 2. 导航滚动态 ---- */
  var ticking = false;
  function onScroll() {
    if (!ticking) {
      window.requestAnimationFrame(function () {
        var y = window.scrollY;
        if (y > 24) nav.classList.add("scrolled");
        else nav.classList.remove("scrolled");

        /* hero 流场视差：随滚动缓慢上移 + 淡出 */
        if (!REDUCE) {
          var flow = document.querySelector(".hero .flow");
          if (flow) {
            var heroH = window.innerHeight;
            var p = Math.min(y / heroH, 1);
            flow.style.transform =
              "translateY(" + (-p * 60).toFixed(1) + "px)";
            flow.style.opacity = String(Math.max(1 - p * 1.3, 0));
          }
        }
        ticking = false;
      });
      ticking = true;
    }
  }
  window.addEventListener("scroll", onScroll, { passive: true });
  onScroll();

  /* ---- 3. 滚动渐入（IntersectionObserver）---- */
  var revealEls = document.querySelectorAll(".reveal, .reveal-group");
  if ("IntersectionObserver" in window && !REDUCE) {
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

  /* ---- 4. 大数字递增（data-count）---- */
  /* 用法：<span data-count="20" data-suffix="k/s">0</span>
     仅在进入视口时触发一次，遵守 reduced-motion（直接显终值） */
  function animateCount(el) {
    var target = parseFloat(el.getAttribute("data-count"));
    var suffix = el.getAttribute("data-suffix") || "";
    var decimals = (el.getAttribute("data-count").split(".")[1] || "").length;
    if (REDUCE || isNaN(target)) {
      el.textContent = target + suffix;
      return;
    }
    var dur = 1400;
    var start = performance.now();
    function step(now) {
      var t = Math.min((now - start) / dur, 1);
      var eased = 1 - Math.pow(1 - t, 3); /* easeOutCubic */
      var val = target * eased;
      el.textContent = val.toFixed(decimals) + suffix;
      if (t < 1) requestAnimationFrame(step);
      else el.textContent = target.toFixed(decimals) + suffix;
    }
    requestAnimationFrame(step);
  }
  var countEls = document.querySelectorAll("[data-count]");
  if (countEls.length) {
    if ("IntersectionObserver" in window) {
      var cio = new IntersectionObserver(function (entries) {
        entries.forEach(function (e) {
          if (e.isIntersecting) {
            animateCount(e.target);
            cio.unobserve(e.target);
          }
        });
      }, { threshold: 0.5 });
      countEls.forEach(function (el) { cio.observe(el); });
    } else {
      countEls.forEach(animateCount);
    }
  }

  /* ---- 5. 页内目录当前节高亮 ---- */
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

  /* ---- 6. 流场 pill 联动：hover 时对应曲线高亮 ---- */
  /* 每个 flow-pill 通过 data-curve 索引关联 flow-curve，hover 联动发光 */
  var flow = document.querySelector(".hero .flow");
  if (flow && !REDUCE) {
    var pills = flow.querySelectorAll(".flow-pill");
    var curves = flow.querySelectorAll(".flow-curve");
    pills.forEach(function (pill, i) {
      pill.style.cursor = "pointer";
      pill.addEventListener("mouseenter", function () {
        /* 让所有曲线临时变亮，被指向的那条更亮 */
        curves.forEach(function (c, ci) {
          c.style.transition = "stroke 240ms ease, opacity 240ms ease";
          c.style.opacity = ci === i ? "1" : "0.25";
        });
      });
      pill.addEventListener("mouseleave", function () {
        curves.forEach(function (c) {
          c.style.opacity = "";
        });
      });
    });
  }
})();
