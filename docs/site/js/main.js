/* nlos core interactions
   Navigation, counters, section spy and the architecture flow diagram live here.
   Visual entrance animation is owned exclusively by effects.js. */
(function () {
  "use strict";

  document.documentElement.classList.add("js");

  var REDUCE_QUERY = window.matchMedia
    ? window.matchMedia("(prefers-reduced-motion: reduce)")
    : { matches: false };
  var DESKTOP_QUERY = window.matchMedia
    ? window.matchMedia("(min-width: 721px)")
    : { matches: true };
  var FINE_POINTER_QUERY = window.matchMedia
    ? window.matchMedia("(pointer: fine)")
    : { matches: false };

  /* ---- Fixed navigation and accessible mobile menu ---- */
  var nav = document.getElementById("nav");
  var navToggle = document.getElementById("navToggle");
  var navLinks = document.getElementById("navLinks");
  var menuOpen = false;
  var navTicking = false;

  function setMenu(open) {
    if (!nav || !navToggle || !navLinks) return;
    menuOpen = Boolean(open && !DESKTOP_QUERY.matches);
    nav.classList.toggle("menu-open", menuOpen);
    navToggle.setAttribute("aria-expanded", String(menuOpen));
    navToggle.setAttribute(
      "aria-label",
      menuOpen ? "关闭主导航" : "打开主导航"
    );
  }

  if (navToggle && navLinks) {
    navToggle.addEventListener("click", function () {
      setMenu(!menuOpen);
    });

    navLinks.addEventListener("click", function (event) {
      if (event.target.closest("a")) setMenu(false);
    });

    document.addEventListener("keydown", function (event) {
      if (event.key === "Escape" && menuOpen) {
        setMenu(false);
        navToggle.focus();
      }
    });

    document.addEventListener("pointerdown", function (event) {
      if (menuOpen && nav && !nav.contains(event.target)) setMenu(false);
    });

    var onBreakpointChange = function () {
      if (DESKTOP_QUERY.matches) setMenu(false);
    };
    if (DESKTOP_QUERY.addEventListener) {
      DESKTOP_QUERY.addEventListener("change", onBreakpointChange);
    } else if (DESKTOP_QUERY.addListener) {
      DESKTOP_QUERY.addListener(onBreakpointChange);
    }
  }

  function updateNav() {
    if (!nav) return;
    nav.classList.toggle("scrolled", window.scrollY > 18);
  }

  function onScroll() {
    if (navTicking) return;
    navTicking = true;
    window.requestAnimationFrame(function () {
      updateNav();
      navTicking = false;
    });
  }

  window.addEventListener("scroll", onScroll, { passive: true });
  updateNav();

  /* ---- Count-up statistics ---- */
  function setFinalCount(element) {
    var raw = element.getAttribute("data-count");
    var target = Number(raw);
    var suffix = element.getAttribute("data-suffix") || "";
    if (!Number.isFinite(target)) return;
    var decimals = (raw.split(".")[1] || "").length;
    element.textContent = target.toFixed(decimals) + suffix;
  }

  function animateCount(element) {
    var raw = element.getAttribute("data-count");
    var target = Number(raw);
    var suffix = element.getAttribute("data-suffix") || "";
    if (!Number.isFinite(target)) return;
    if (REDUCE_QUERY.matches) {
      setFinalCount(element);
      return;
    }

    var decimals = (raw.split(".")[1] || "").length;
    var duration = 1050;
    var start = performance.now();

    function step(now) {
      var progress = Math.min((now - start) / duration, 1);
      var eased = 1 - Math.pow(1 - progress, 3);
      element.textContent = (target * eased).toFixed(decimals) + suffix;
      if (progress < 1) {
        window.requestAnimationFrame(step);
      } else {
        setFinalCount(element);
      }
    }

    window.requestAnimationFrame(step);
  }

  var countElements = Array.prototype.slice.call(
    document.querySelectorAll("[data-count]")
  );

  if (countElements.length) {
    if (!("IntersectionObserver" in window) || REDUCE_QUERY.matches) {
      countElements.forEach(setFinalCount);
    } else {
      var countObserver = new IntersectionObserver(
        function (entries) {
          entries.forEach(function (entry) {
            if (!entry.isIntersecting) return;
            animateCount(entry.target);
            countObserver.unobserve(entry.target);
          });
        },
        { threshold: 0.45 }
      );
      countElements.forEach(function (element) {
        countObserver.observe(element);
      });
    }
  }

  /* ---- In-page table-of-contents spy ---- */
  var tocLinks = Array.prototype.slice.call(
    document.querySelectorAll(".page-toc a[href^='#']")
  );
  var tocById = {};
  var tocSections = [];

  tocLinks.forEach(function (link) {
    var id = link.getAttribute("href").slice(1);
    var section = document.getElementById(id);
    if (!section) return;
    tocById[id] = link;
    tocSections.push(section);
  });

  function activateToc(id) {
    tocLinks.forEach(function (link) {
      link.classList.toggle("active", link === tocById[id]);
    });
  }

  if ("IntersectionObserver" in window && tocSections.length) {
    var visibleSections = {};
    var tocObserver = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            visibleSections[entry.target.id] = Math.abs(
              entry.boundingClientRect.top
            );
          } else {
            delete visibleSections[entry.target.id];
          }
        });

        var activeId = Object.keys(visibleSections).sort(function (a, b) {
          return visibleSections[a] - visibleSections[b];
        })[0];
        if (activeId) activateToc(activeId);
      },
      { rootMargin: "-18% 0px -68% 0px", threshold: 0.01 }
    );

    tocSections.forEach(function (section) {
      tocObserver.observe(section);
    });
  }

  /* ---- Architecture flow hover linkage ---- */
  var flow = document.querySelector(".hero .flow");
  if (flow && FINE_POINTER_QUERY.matches && !REDUCE_QUERY.matches) {
    var curves = Array.prototype.slice.call(
      flow.querySelectorAll(".flow-curve")
    );
    var pills = Array.prototype.slice.call(flow.querySelectorAll(".flow-pill"));

    pills.forEach(function (pill) {
      pill.addEventListener("pointerenter", function () {
        var targetIndex = Math.max(
          0,
          Number(pill.getAttribute("data-curve") || 1) - 1
        );
        curves.forEach(function (curve, index) {
          curve.classList.toggle("is-highlighted", index === targetIndex);
          curve.classList.toggle("is-muted", index !== targetIndex);
        });
      });

      pill.addEventListener("pointerleave", function () {
        curves.forEach(function (curve) {
          curve.classList.remove("is-highlighted", "is-muted");
        });
      });
    });
  }

  /* ---- Interactive idea maps ---- */
  Array.prototype.slice
    .call(document.querySelectorAll("[data-idea-map]"))
    .forEach(function (map) {
      var nodes = Array.prototype.slice.call(
        map.querySelectorAll("[data-map-node]")
      );
      var paths = Array.prototype.slice.call(
        map.querySelectorAll("[data-map-link]")
      );
      var readout = map.querySelector(".idea-map-readout");
      var readoutIndex = readout
        ? readout.querySelector(".idea-map-index")
        : null;
      var readoutTitle = readout ? readout.querySelector("strong") : null;
      var readoutCopy = readout ? readout.querySelector("p") : null;
      var activeIndex = Math.max(
        0,
        nodes.findIndex(function (node) {
          return node.classList.contains("is-active");
        })
      );
      var paused = false;
      var pinned = false;

      function activate(index) {
        activeIndex = (index + nodes.length) % nodes.length;
        var node = nodes[activeIndex];
        var nodeId = node.getAttribute("data-map-node");

        nodes.forEach(function (candidate, candidateIndex) {
          var active = candidateIndex === activeIndex;
          candidate.classList.toggle("is-active", active);
          candidate.setAttribute("aria-pressed", String(active));
        });

        paths.forEach(function (path) {
          var linkedIds = (
            path.getAttribute("data-map-link") || ""
          ).split(/\s+/);
          path.classList.toggle("is-active", linkedIds.indexOf(nodeId) >= 0);
        });

        if (readoutIndex) {
          readoutIndex.textContent = String(activeIndex + 1).padStart(2, "0");
        }
        if (readoutTitle) {
          readoutTitle.textContent =
            node.getAttribute("data-map-title") ||
            node.querySelector("strong").textContent;
        }
        if (readoutCopy) {
          readoutCopy.textContent =
            node.getAttribute("data-map-copy") || "";
        }
      }

      nodes.forEach(function (node, index) {
        node.setAttribute("aria-pressed", "false");
        node.addEventListener("pointerenter", function () {
          paused = true;
          activate(index);
        });
        node.addEventListener("focus", function () {
          paused = true;
          activate(index);
        });
        node.addEventListener("click", function () {
          pinned = !pinned || activeIndex !== index;
          paused = pinned;
          map.classList.toggle("is-pinned", pinned);
          activate(index);
        });
      });

      map.addEventListener("pointerleave", function () {
        if (!pinned) paused = false;
      });
      map.addEventListener("focusout", function (event) {
        if (!map.contains(event.relatedTarget) && !pinned) paused = false;
      });

      activate(activeIndex);

      if (
        !REDUCE_QUERY.matches &&
        FINE_POINTER_QUERY.matches &&
        nodes.length > 1
      ) {
        window.setInterval(function () {
          if (paused || document.hidden) return;
          activate(activeIndex + 1);
        }, 4200);
      }
    });

  /* ---- Focus linkage for existing architecture structures ---- */
  var structures = Array.prototype.slice.call(
    document.querySelectorAll(
      ".arch-stack, .topo, .tier-ladder, .trace-ladder"
    )
  );

  structures.forEach(function (structure) {
    var selector = ".arch-layer, .tier, .trace-row";
    if (structure.classList.contains("topo")) {
      selector = ".arch-layer, .grid-3 > .card";
    }
    var items = Array.prototype.slice
      .call(structure.querySelectorAll(selector))
      .filter(function (item) {
        return item.closest(
          ".arch-stack, .topo, .tier-ladder, .trace-ladder"
        ) === structure;
      });

    if (items.length < 2) return;
    structure.classList.add("interactive-structure");

    function focusItem(activeItem) {
      structure.classList.add("has-diagram-focus");
      items.forEach(function (item) {
        item.classList.toggle("is-diagram-active", item === activeItem);
      });
    }

    function clearFocus() {
      structure.classList.remove("has-diagram-focus");
      items.forEach(function (item) {
        item.classList.remove("is-diagram-active");
      });
    }

    items.forEach(function (item) {
      item.classList.add("diagram-item");
      item.setAttribute("tabindex", "0");
      item.addEventListener("pointerenter", function () {
        focusItem(item);
      });
      item.addEventListener("focus", function () {
        focusItem(item);
      });
    });

    structure.addEventListener("pointerleave", clearFocus);
    structure.addEventListener("focusout", function (event) {
      if (!structure.contains(event.relatedTarget)) clearFocus();
    });
  });

  /* ---- 右侧悬浮中文导航面板 + 右缘滚动进度细轨 ----
     渐进增强：全部由 JS 生成，无 JS 时页面无残留占位。
     移动端（<1024px）面板由 CSS 隐藏，进度轨保留。 */
  var railSections = Array.prototype.slice.call(
    document.querySelectorAll("main section[id]")
  );

  if (railSections.length) {
    /* 中文标签字典：按 section id 映射，字典外 id fallback 到 h2 前 6 字 */
    var CN_LABELS = {
      top: "顶部",
      /* index */
      history: "历史论证",
      problem: "问题",
      direction: "方向修正",
      tour: "阅读路线",
      /* kernel */
      arch: "三层架构",
      abstractions: "八大抽象",
      syscalls: "系统调用",
      elf: "ELF 契约",
      universality: "通用性论证",
      /* verification */
      budget: "Budget 经济",
      randomness: "随机性管理",
      security: "重授权签字链",
      validation: "宗旨验证",
      /* modern */
      namespace: "命名即权限",
      distributed: "分布式内核",
      protocol: "协议栈",
      elasticity: "弹性多租户",
      observability: "可观测性",
      principles: "设计原则"
    };

    function sectionLabel(section) {
      if (CN_LABELS[section.id]) return CN_LABELS[section.id];
      var h2 = section.querySelector("h2");
      if (h2) {
        var fallback = h2.textContent.trim().slice(0, 6);
        if (fallback) return fallback;
      }
      return section.id;
    }

    var panel = document.createElement("nav");
    panel.className = "side-nav";
    panel.setAttribute("aria-label", "章节导航");

    var itemById = {};

    function addItem(id, label) {
      var link = document.createElement("a");
      link.href = "#" + id;
      link.textContent = label;
      itemById[id] = link;
      panel.appendChild(link);
    }

    addItem("top", CN_LABELS.top);
    railSections.forEach(function (section) {
      addItem(section.id, sectionLabel(section));
    });
    // 节点大小渐变：两端 1.0 → 正中间 0.6，写入内联 --node-scale（只瘦星形节点，不瘦文字）
    var navLinks = panel.querySelectorAll("a");
    var midIndex = (navLinks.length - 1) / 2;
    navLinks.forEach(function (link, i) {
      var t = midIndex > 0 ? Math.abs(i - midIndex) / midIndex : 0;
      link.style.setProperty("--node-scale", (1 - 0.4 * (1 - t)).toFixed(3));
    });
    document.body.appendChild(panel);

    var progress = document.createElement("div");
    progress.className = "scroll-progress";
    progress.setAttribute("aria-hidden", "true");
    var progressFill = document.createElement("div");
    progressFill.className = "scroll-progress-fill";
    progress.appendChild(progressFill);
    document.body.appendChild(progress);

    var activeItemId = null;
    function setActiveItem(id) {
      if (id === activeItemId) return;
      activeItemId = id;
      Object.keys(itemById).forEach(function (key) {
        var active = key === id;
        itemById[key].classList.toggle("active", active);
        if (active) {
          itemById[key].setAttribute("aria-current", "true");
        } else {
          itemById[key].removeAttribute("aria-current");
        }
      });
    }

    function updateProgress() {
      var doc = document.documentElement;
      var max = doc.scrollHeight - window.innerHeight;
      var ratio =
        max > 0 ? Math.min(1, Math.max(0, window.scrollY / max)) : 0;
      progressFill.style.transform = "scaleY(" + ratio + ")";
    }

    function updateSpy() {
      var y = window.scrollY;
      var probe = y + window.innerHeight * 0.4;
      var current = "top";
      for (var i = 0; i < railSections.length; i++) {
        var top =
          railSections[i].getBoundingClientRect().top + y;
        if (top <= probe) current = railSections[i].id;
      }
      /* 滚到页面底部时锁定最后一个 section */
      if (
        y + window.innerHeight >=
        document.documentElement.scrollHeight - 2
      ) {
        current = railSections[railSections.length - 1].id;
      }
      setActiveItem(current);
    }

    var sideTicking = false;
    function onSideScroll() {
      if (sideTicking) return;
      sideTicking = true;
      window.requestAnimationFrame(function () {
        updateProgress();
        updateSpy();
        sideTicking = false;
      });
    }

    window.addEventListener("scroll", onSideScroll, { passive: true });
    updateProgress();
    updateSpy();
  }

  /* v10 · 高密度内容默认只展示核心层，其余语料进入可逆的详细层。 */
  document
    .querySelectorAll(".autopsy-grid, .tiers, .principles, .stats, .tl-notes")
    .forEach(function (grid, gridIndex) {
      var items = Array.prototype.filter.call(grid.children, function (child) {
        return child.nodeType === 1;
      });
      var visibleCount = grid.classList.contains("stats") ? 4 : 3;
      if (items.length <= visibleCount) return;

      var hiddenCount = items.length - visibleCount;
      items.slice(visibleCount).forEach(function (item) {
        item.classList.add("density-extra");
      });
      grid.classList.add("density-grid", "is-collapsed");
      grid.id = grid.id || "density-grid-" + gridIndex;

      var toggle = document.createElement("button");
      toggle.type = "button";
      toggle.className = "density-toggle";
      toggle.setAttribute("aria-controls", grid.id);
      toggle.setAttribute("aria-expanded", "false");
      toggle.innerHTML =
        "<span>展开详细层</span><small>+" + hiddenCount + " ITEMS</small>";

      toggle.addEventListener("click", function () {
        var expanded = toggle.getAttribute("aria-expanded") === "true";
        toggle.setAttribute("aria-expanded", String(!expanded));
        grid.classList.toggle("is-collapsed", expanded);
        toggle.querySelector("span").textContent = expanded
          ? "展开详细层"
          : "收起详细层";
      });
      grid.insertAdjacentElement("afterend", toggle);
    });
})();
