/* ==========================================================================
   NLOS Documentation & Product Site — Core Interactive Shell v11.0
   Zero external runtime dependencies. Offline & GitHub Pages ready.
   ========================================================================== */
(function () {
  'use strict';

  var docEl = document.documentElement;
  docEl.classList.add('js');

  var reduceMotion =
    window.matchMedia &&
    window.matchMedia('(prefers-reduced-motion: reduce)').matches;

  /* ------------------------------------------------------------------------
     1. Top Navigation Bar & Mobile Drawer
     ------------------------------------------------------------------------ */
  var nav = document.getElementById('nav');
  var navToggle = document.getElementById('navToggle');
  var navLinks = document.getElementById('navLinks');

  function syncNavScrollState() {
    if (!nav) return;
    nav.classList.toggle('scrolled', window.scrollY > 18);
  }

  function closeMobileMenu() {
    if (!nav || !navToggle) return;
    nav.classList.remove('menu-open');
    navToggle.setAttribute('aria-expanded', 'false');
    navToggle.setAttribute('aria-label', '打开导航菜单');
  }

  function initNavigation() {
    syncNavScrollState();
    window.addEventListener('scroll', syncNavScrollState, { passive: true });

    if (!nav || !navToggle || !navLinks) return;

    navToggle.addEventListener('click', function () {
      var willOpen = !nav.classList.contains('menu-open');
      nav.classList.toggle('menu-open', willOpen);
      navToggle.setAttribute('aria-expanded', willOpen ? 'true' : 'false');
      navToggle.setAttribute(
        'aria-label',
        willOpen ? '关闭导航菜单' : '打开导航菜单'
      );
    });

    navLinks.querySelectorAll('a').forEach(function (link) {
      link.addEventListener('click', closeMobileMenu);
    });

    document.addEventListener('click', function (event) {
      if (nav.classList.contains('menu-open') && !nav.contains(event.target)) {
        closeMobileMenu();
      }
    });

    document.addEventListener('keydown', function (event) {
      if (event.key === 'Escape' && nav.classList.contains('menu-open')) {
        closeMobileMenu();
        navToggle.focus();
      }
    });

    window.addEventListener(
      'resize',
      function () {
        if (window.innerWidth > 980 && nav.classList.contains('menu-open')) {
          closeMobileMenu();
        }
      },
      { passive: true }
    );
  }

  /* ------------------------------------------------------------------------
     2. Smooth In-Page Anchor Scrolling (Sticky Header Offset Aware)
     ------------------------------------------------------------------------ */
  function initSmoothAnchors() {
    document.addEventListener('click', function (event) {
      var anchor = event.target.closest && event.target.closest('a[href^="#"]');
      if (!anchor) return;
      var href = anchor.getAttribute('href');
      if (!href || href === '#' || href.length < 2) return;

      var id = decodeURIComponent(href.slice(1));
      var target = document.getElementById(id);
      if (!target) return;

      event.preventDefault();
      closeMobileMenu();

      var navHeight = nav ? nav.getBoundingClientRect().height : 68;
      var top =
        target.getBoundingClientRect().top + window.scrollY - navHeight - 18;

      window.scrollTo({
        top: Math.max(0, top),
        behavior: reduceMotion ? 'auto' : 'smooth'
      });

      if (window.history && window.history.pushState) {
        window.history.pushState(null, '', '#' + encodeURIComponent(id));
      }
    });
  }

  /* ------------------------------------------------------------------------
     3. Telemetry Number Count-Up ([data-count])
     ------------------------------------------------------------------------ */
  function initCountUp() {
    var nodes = Array.prototype.slice.call(
      document.querySelectorAll('[data-count]')
    );
    if (!nodes.length) return;

    function formatValue(val, decimals, prefix, suffix) {
      var str = decimals > 0 ? val.toFixed(decimals) : String(Math.round(val));
      return prefix + str + suffix;
    }

    function animateNode(el) {
      if (el.dataset.counted === '1') return;
      el.dataset.counted = '1';

      var raw = el.getAttribute('data-count') || '0';
      var target = parseFloat(raw);
      if (!isFinite(target)) return;

      var decimals = raw.indexOf('.') >= 0 ? raw.split('.')[1].length : 0;
      var prefix = el.getAttribute('data-prefix') || '';
      var suffix = el.getAttribute('data-suffix') || '';

      if (reduceMotion) {
        el.textContent = formatValue(target, decimals, prefix, suffix);
        return;
      }

      var duration = 880;
      var start = performance.now();

      function tick(now) {
        var progress = Math.min(1, (now - start) / duration);
        var eased = 1 - Math.pow(1 - progress, 3);
        el.textContent = formatValue(target * eased, decimals, prefix, suffix);
        if (progress < 1) {
          requestAnimationFrame(tick);
        } else {
          el.textContent = formatValue(target, decimals, prefix, suffix);
        }
      }

      requestAnimationFrame(tick);
    }

    if (!('IntersectionObserver' in window) || reduceMotion) {
      nodes.forEach(animateNode);
      return;
    }

    var observer = new IntersectionObserver(
      function (entries) {
        entries.forEach(function (entry) {
          if (entry.isIntersecting) {
            animateNode(entry.target);
            observer.unobserve(entry.target);
          }
        });
      },
      { threshold: 0.35 }
    );

    nodes.forEach(function (node) {
      observer.observe(node);
    });
  }

  /* ------------------------------------------------------------------------
     4. Home Gerono Dual-Comet Lemniscate Emblem (canvas.emblem-infinity)
     ------------------------------------------------------------------------ */
  function initInfinityEmblem() {
    var canvas = document.querySelector('.emblem-infinity');
    if (!canvas || !canvas.getContext) return;
    var ctx = canvas.getContext('2d');
    if (!ctx) return;

    var dpr = 1;
    var width = 0;
    var height = 0;
    var rafId = 0;
    var inViewport = true;
    var PERIOD = 4.8;
    var COMET_SEGMENTS = 62;
    var TAIL_SPAN = 1.48;
    var STATIC_TIME = 0.55;
    var NODE_COUNT = 28;
    var microNodes = [];

    for (var i = 0; i < NODE_COUNT; i++) {
      microNodes.push({
        t: (i / NODE_COUNT) * Math.PI * 2,
        radial: (((i * 37) % 11) - 5) * 0.55,
        radius: i % 7 === 0 ? 1.45 : i % 3 === 0 ? 1.05 : 0.78,
        alpha: 0.22 + ((i * 19) % 36) / 100
      });
    }

    function geronoPoint(t, A, B) {
      return {
        x: A * Math.cos(t),
        y: B * Math.sin(2 * t)
      };
    }

    function resize() {
      var rect = canvas.getBoundingClientRect();
      width = Math.max(1, rect.width);
      height = Math.max(1, rect.height);
      dpr = Math.min(window.devicePixelRatio || 1, 2);
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    }

    function renderFrame(timeSec) {
      if (!width || !height) resize();
      ctx.clearRect(0, 0, width, height);

      var cx = width * 0.5;
      var cy = height * 0.5;
      var A = Math.min(width * 0.42, 184);
      var B = A * 0.38;

      // 1. Base closed Gerono curve + soft outer glacier glow
      ctx.save();
      ctx.beginPath();
      var steps = 160;
      for (var k = 0; k <= steps; k++) {
        var u = (k / steps) * Math.PI * 2;
        var pt = geronoPoint(u, A, B);
        if (k === 0) ctx.moveTo(cx + pt.x, cy + pt.y);
        else ctx.lineTo(cx + pt.x, cy + pt.y);
      }
      ctx.closePath();
      ctx.lineJoin = 'round';
      ctx.lineCap = 'round';

      ctx.strokeStyle = 'rgba(138, 212, 255, 0.08)';
      ctx.lineWidth = 5;
      ctx.stroke();

      ctx.strokeStyle = 'rgba(164, 222, 255, 0.20)';
      ctx.lineWidth = 1.15;
      ctx.stroke();
      ctx.restore();

      // 2. Subtle deterministic starlight micro-nodes along the Gerono path
      for (var n = 0; n < microNodes.length; n++) {
        var node = microNodes[n];
        var nPt = geronoPoint(node.t, A, B);
        var nx = cx + nPt.x;
        var ny = cy + nPt.y + node.radial;
        var pulse = reduceMotion
          ? 1
          : 0.78 + 0.22 * Math.sin(timeSec * 1.4 + node.t * 2);
        var nAlpha = node.alpha * pulse;

        ctx.beginPath();
        ctx.arc(nx, ny, node.radius, 0, Math.PI * 2);
        ctx.fillStyle =
          node.radius > 1.2
            ? 'rgba(237, 248, 255, ' + nAlpha.toFixed(3) + ')'
            : 'rgba(138, 212, 255, ' + nAlpha.toFixed(3) + ')';
        ctx.fill();
      }

      // 3. Dual center-symmetric 62-segment comet tails: P2 = 2C - P1
      var headT = (((timeSec % PERIOD) + PERIOD) % PERIOD / PERIOD) * Math.PI * 2;
      ctx.save();
      ctx.lineCap = 'round';

      for (var s = 0; s < COMET_SEGMENTS; s++) {
        var u0 = headT - TAIL_SPAN * (1 - s / COMET_SEGMENTS);
        var u1 = headT - TAIL_SPAN * (1 - (s + 1) / COMET_SEGMENTS);
        var p0 = geronoPoint(u0, A, B);
        var p1 = geronoPoint(u1, A, B);

        var x1a = cx + p0.x;
        var y1a = cy + p0.y;
        var x1b = cx + p1.x;
        var y1b = cy + p1.y;

        // Exact center-inversion P(pi - t) = 2C - P(t)
        var x2a = 2 * cx - x1a;
        var y2a = 2 * cy - y1a;
        var x2b = 2 * cx - x1b;
        var y2b = 2 * cy - y1b;

        var prog = (s + 1) / COMET_SEGMENTS;
        var ease = Math.pow(prog, 2.15);
        var alpha = 0.03 + 0.92 * ease;
        var lw = 0.85 + 2.05 * ease;

        ctx.lineWidth = lw;
        ctx.strokeStyle =
          prog > 0.78
            ? 'rgba(247, 252, 255, ' + alpha.toFixed(3) + ')'
            : 'rgba(138, 212, 255, ' + alpha.toFixed(3) + ')';

        ctx.beginPath();
        ctx.moveTo(x1a, y1a);
        ctx.lineTo(x1b, y1b);
        ctx.moveTo(x2a, y2a);
        ctx.lineTo(x2b, y2b);
        ctx.stroke();
      }

      // Crisp dual comet-head starlight cores
      var headPt = geronoPoint(headT, A, B);
      var hx1 = cx + headPt.x;
      var hy1 = cy + headPt.y;
      var hx2 = 2 * cx - hx1;
      var hy2 = 2 * cy - hy1;

      ctx.fillStyle = 'rgba(138, 212, 255, 0.28)';
      ctx.beginPath();
      ctx.arc(hx1, hy1, 4.4, 0, Math.PI * 2);
      ctx.arc(hx2, hy2, 4.4, 0, Math.PI * 2);
      ctx.fill();

      ctx.fillStyle = 'rgba(255, 255, 255, 0.96)';
      ctx.beginPath();
      ctx.arc(hx1, hy1, 1.65, 0, Math.PI * 2);
      ctx.arc(hx2, hy2, 1.65, 0, Math.PI * 2);
      ctx.fill();

      ctx.restore();
      canvas.classList.add('is-rendered');
    }

    function loop(now) {
      rafId = 0;
      if (reduceMotion || document.hidden || !inViewport) return;
      renderFrame(now * 0.001);
      rafId = requestAnimationFrame(loop);
    }

    function startLoop() {
      if (reduceMotion) {
        renderFrame(STATIC_TIME);
        return;
      }
      if (!rafId && !document.hidden && inViewport) {
        rafId = requestAnimationFrame(loop);
      }
    }

    resize();
    renderFrame(STATIC_TIME);
    if (!reduceMotion) {
      startLoop();
    }

    window.addEventListener(
      'resize',
      function () {
        resize();
        renderFrame(reduceMotion ? STATIC_TIME : performance.now() * 0.001);
      },
      { passive: true }
    );

    if (!reduceMotion) {
      document.addEventListener('visibilitychange', function () {
        if (document.hidden) {
          if (rafId) {
            cancelAnimationFrame(rafId);
            rafId = 0;
          }
        } else {
          startLoop();
        }
      });

      if ('IntersectionObserver' in window) {
        var io = new IntersectionObserver(
          function (entries) {
            entries.forEach(function (entry) {
              inViewport = entry.isIntersecting;
              if (inViewport) startLoop();
            });
          },
          { threshold: 0.02 }
        );
        io.observe(canvas);
      }
    }
  }

  /* ------------------------------------------------------------------------
     5. Interactive Idea Map ([data-idea-map])
     ------------------------------------------------------------------------ */
  function initIdeaMaps() {
    var maps = document.querySelectorAll('[data-idea-map]');
    maps.forEach(function (map) {
      var nodes = Array.prototype.slice.call(
        map.querySelectorAll('[data-map-node]')
      );
      if (!nodes.length) return;

      var links = Array.prototype.slice.call(
        map.querySelectorAll('[data-map-link]')
      );
      var readout = map.querySelector('.idea-map-readout');
      var idxEl = readout ? readout.querySelector('.idea-map-index') : null;
      var titleEl = readout ? readout.querySelector('h4') : null;
      var descEl = readout ? readout.querySelector('p') : null;

      function activate(node) {
        var key = node.getAttribute('data-map-node');
        nodes.forEach(function (item) {
          var active = item === node;
          item.classList.toggle('is-active', active);
          item.setAttribute('aria-pressed', active ? 'true' : 'false');
        });

        links.forEach(function (link) {
          var keys = (link.getAttribute('data-map-link') || '').split(/\s+/);
          link.classList.toggle('is-active', keys.indexOf(key) >= 0);
        });

        if (idxEl) {
          var noEl = node.querySelector('.idea-node-no');
          idxEl.textContent =
            node.getAttribute('data-index') ||
            (noEl ? noEl.textContent.trim() : '');
        }
        if (titleEl && node.getAttribute('data-title')) {
          titleEl.textContent = node.getAttribute('data-title');
        }
        if (descEl && node.getAttribute('data-desc')) {
          descEl.textContent = node.getAttribute('data-desc');
        }
      }

      nodes.forEach(function (node, idx) {
        node.addEventListener('mouseenter', function () {
          activate(node);
        });
        node.addEventListener('focus', function () {
          activate(node);
        });
        node.addEventListener('click', function () {
          activate(node);
        });
        node.addEventListener('keydown', function (event) {
          var nextIdx = -1;
          if (event.key === 'ArrowRight' || event.key === 'ArrowDown') {
            nextIdx = (idx + 1) % nodes.length;
          } else if (event.key === 'ArrowLeft' || event.key === 'ArrowUp') {
            nextIdx = (idx - 1 + nodes.length) % nodes.length;
          }
          if (nextIdx >= 0) {
            event.preventDefault();
            nodes[nextIdx].focus();
            activate(nodes[nextIdx]);
          }
        });
      });

      var initial =
        map.querySelector('[data-map-node].is-active') || nodes[0];
      if (initial) activate(initial);
    });
  }

  /* ------------------------------------------------------------------------
     6. Interactive Diagram Focus (.interactive-structure)
     ------------------------------------------------------------------------ */
  function initInteractiveStructures() {
    var ITEM_SELECTOR = [
      '.arch-layer',
      '.abs-card',
      '.tier',
      '.stack-layer',
      '.control-row',
      '.graph-node',
      '.commit-lane',
      '.budget-cell',
      '.resource-node',
      '.resource-core',
      '.bento-card',
      '.stage-card',
      '.wp-item',
      '.cell-node',
      '.cell-host',
      '.crate-plane',
      '.h-row',
      '.h-step'
    ].join(', ');

    document.querySelectorAll('.interactive-structure').forEach(function (root) {
      var items = Array.prototype.slice.call(root.querySelectorAll(ITEM_SELECTOR));
      if (items.length < 2) return;

      function setFocus(activeItem) {
        root.classList.toggle('has-focus', Boolean(activeItem));
        items.forEach(function (el) {
          el.classList.toggle('is-focused', el === activeItem);
        });
      }

      items.forEach(function (item) {
        item.addEventListener('mouseenter', function () {
          setFocus(item);
        });
        item.addEventListener('focusin', function () {
          setFocus(item);
        });
      });

      root.addEventListener('mouseleave', function () {
        setFocus(null);
      });
      root.addEventListener('focusout', function (event) {
        if (!root.contains(event.relatedTarget)) {
          setFocus(null);
        }
      });
    });
  }

  /* ------------------------------------------------------------------------
     7. Section Label Extraction, Scroll Telemetry, Global Route & Handoffs
     ------------------------------------------------------------------------ */
  function shortNavLabel(section) {
    if (!section) return '';

    // 1. Explicit data-nav-label on section
    var explicit = section.getAttribute('data-nav-label');
    if (explicit && explicit.trim()) return explicit.trim();

    // 2. Curated label in .page-toc matching #section.id
    if (section.id) {
      var tocLink = document.querySelector(
        '.page-toc a[href="#' + CSS.escape(section.id) + '"]'
      );
      if (tocLink) {
        var tocText = tocLink.textContent
          .replace(/^\s*\d+\s*[\.·/|-]?\s*/, '')
          .trim();
        if (tocText) return tocText;
      }
    }

    // 3. Explicit data-short on heading
    var h2 = section.querySelector('.sec-head h2, h2');
    if (h2) {
      var h2Short = h2.getAttribute('data-short');
      if (h2Short && h2Short.trim()) return h2Short.trim();

      var raw = h2.textContent.replace(/\s+/g, ' ').trim();
      // Extract clean lead clause before colon / em-dash / pipe if concise
      var leadMatch = raw.split(/[：:—｜|]/)[0].trim();
      if (leadMatch && leadMatch.length >= 2 && leadMatch.length <= 13) {
        return leadMatch;
      }
      if (raw.length <= 12) return raw;
      return raw.slice(0, 11).replace(/[，、·\s]+$/, '') + '…';
    }

    // 4. Fallback to eyebrow text without numeric counter
    var eyebrow = section.querySelector('.eyebrow');
    if (eyebrow) {
      var clone = eyebrow.cloneNode(true);
      var secNo = clone.querySelector('.sec-no');
      if (secNo) secNo.remove();
      var eyeText = clone.textContent.replace(/\s+/g, ' ').trim();
      if (eyeText) {
        return eyeText.length <= 14 ? eyeText : eyeText.slice(0, 13) + '…';
      }
    }

    return section.id || '';
  }

  function initScrollTelemetry() {
    var sections = Array.prototype.slice.call(
      document.querySelectorAll('main .section[id]')
    );
    var tocLinks = Array.prototype.slice.call(
      document.querySelectorAll('.page-toc a[href^="#"]')
    );

    // Inject right-edge vertical progress bar
    var progressBar = document.querySelector('.scroll-progress');
    var progressFill = null;
    if (!progressBar) {
      progressBar = document.createElement('div');
      progressBar.className = 'scroll-progress';
      progressBar.setAttribute('aria-hidden', 'true');
      progressFill = document.createElement('div');
      progressFill.className = 'scroll-progress-bar';
      progressBar.appendChild(progressFill);
      document.body.appendChild(progressBar);
    } else {
      progressFill = progressBar.querySelector(
        '.scroll-progress-bar, .scroll-progress-fill'
      );
    }

    // Inject right-edge constellation .side-nav when >= 2 anchored sections exist
    var sideLinks = [];
    var sideNav = document.querySelector('.side-nav');
    if (!sideNav && sections.length >= 2) {
      sideNav = document.createElement('nav');
      sideNav.className = 'side-nav';
      sideNav.setAttribute('aria-label', '本页章节导航');

      sections.forEach(function (sec, index) {
        var label = shortNavLabel(sec);
        if (!label) return;
        var a = document.createElement('a');
        a.href = '#' + sec.id;
        a.dataset.sectionId = sec.id;
        a.style.setProperty(
          '--node-scale',
          index === 0 ? '1.08' : (0.9 + (index % 3) * 0.06).toFixed(2)
        );
        var span = document.createElement('span');
        span.textContent = label;
        a.appendChild(span);
        sideNav.appendChild(a);
        sideLinks.push(a);
      });

      if (sideLinks.length >= 2) {
        document.body.appendChild(sideNav);
      }
    } else if (sideNav) {
      sideLinks = Array.prototype.slice.call(sideNav.querySelectorAll('a'));
    }

    var ticking = false;
    function updateTelemetry() {
      ticking = false;
      var scrollTop = window.scrollY || docEl.scrollTop || 0;
      var maxScroll = Math.max(1, docEl.scrollHeight - window.innerHeight);
      var ratio = Math.min(1, Math.max(0, scrollTop / maxScroll));

      if (progressFill) {
        progressFill.style.transform = 'scaleY(' + ratio.toFixed(4) + ')';
      }

      if (!sections.length) return;

      var probeY = scrollTop + window.innerHeight * 0.3;
      var activeId = sections[0].id;
      for (var i = 0; i < sections.length; i++) {
        var top = sections[i].getBoundingClientRect().top + scrollTop;
        if (top <= probeY) {
          activeId = sections[i].id;
        }
      }

      sideLinks.forEach(function (link) {
        var isMatch = link.getAttribute('href') === '#' + activeId;
        link.classList.toggle('active', isMatch);
        if (isMatch) link.setAttribute('aria-current', 'location');
        else link.removeAttribute('aria-current');
      });

      tocLinks.forEach(function (link) {
        var isMatch = link.getAttribute('href') === '#' + activeId;
        link.classList.toggle('active', isMatch);
      });
    }

    function requestTick() {
      if (!ticking) {
        ticking = true;
        requestAnimationFrame(updateTelemetry);
      }
    }

    updateTelemetry();
    window.addEventListener('scroll', requestTick, { passive: true });
    window.addEventListener('resize', requestTick, { passive: true });
  }

  function initSystemRoute() {
    var pageHeadContainer = document.querySelector('.page-head .container');
    if (!pageHeadContainer) return;
    if (pageHeadContainer.querySelector('.system-route')) return;

    var routes = [
      { no: '00', label: '首页总览', href: 'index.html' },
      { no: '01', label: '系统哲学', href: 'vision.html' },
      { no: '02', label: '通用内核', href: 'kernel.html' },
      { no: '03', label: '执行与控制', href: 'execution.html' },
      { no: '04', label: '支撑与验证', href: 'verification.html' },
      { no: '05', label: '现代系统', href: 'modern.html' },
      { no: '06', label: '阶段路线', href: 'roadmap.html' }
    ];

    var path = window.location.pathname || '';
    var currentFile = path.split('/').pop() || 'index.html';

    var strip = document.createElement('nav');
    strip.className = 'system-route';
    strip.setAttribute('aria-label', '全站章节路由');

    routes.forEach(function (item) {
      var a = document.createElement('a');
      a.href = item.href;
      var isCurrent = currentFile === item.href;
      if (isCurrent) {
        a.className = 'is-current';
        a.setAttribute('aria-current', 'page');
      }
      var small = document.createElement('small');
      small.textContent = item.no;
      var span = document.createElement('span');
      span.textContent = item.label;
      a.appendChild(small);
      a.appendChild(span);
      strip.appendChild(a);
    });

    pageHeadContainer.appendChild(strip);
  }

  function initSectionHandoffs() {
    var sections = Array.prototype.slice.call(
      document.querySelectorAll('main > .section[id]')
    );
    if (sections.length < 2) return;

    for (var i = 1; i < sections.length; i++) {
      var prev = sections[i - 1];
      var curr = sections[i];
      if (curr.querySelector(':scope > .section-handoff')) continue;

      var prevLabel = shortNavLabel(prev);
      var currLabel = shortNavLabel(curr);
      if (!prevLabel || !currLabel) continue;

      var handoff = document.createElement('div');
      handoff.className = 'section-handoff';
      handoff.setAttribute('aria-hidden', 'true');

      var sPrev = document.createElement('span');
      sPrev.textContent = prevLabel;
      var lineLeft = document.createElement('i');
      var arrow = document.createElement('b');
      arrow.textContent = '↓';
      var lineRight = document.createElement('i');
      var sCurr = document.createElement('span');
      sCurr.textContent = currLabel;

      handoff.appendChild(sPrev);
      handoff.appendChild(lineLeft);
      handoff.appendChild(arrow);
      handoff.appendChild(lineRight);
      handoff.appendChild(sCurr);

      curr.insertBefore(handoff, curr.firstChild);
    }
  }

  /* ------------------------------------------------------------------------
     8. Code / CLI / Manifest Tabbed Showcase & Copy Button (v11.0 #5)
     ------------------------------------------------------------------------ */
  function initCodeTabs() {
    var groups = document.querySelectorAll('.code-showcase, [data-tabs]');
    groups.forEach(function (group) {
      var tabs = Array.prototype.slice.call(
        group.querySelectorAll('.code-tab, [role="tab"]')
      );
      var panels = Array.prototype.slice.call(
        group.querySelectorAll('.code-panel, [role="tabpanel"]')
      );
      if (!tabs.length || !panels.length) return;

      function selectTab(nextIndex, moveFocus) {
        tabs.forEach(function (tab, i) {
          var active = i === nextIndex;
          tab.classList.toggle('is-active', active);
          tab.setAttribute('aria-selected', active ? 'true' : 'false');
          tab.setAttribute('tabindex', active ? '0' : '-1');
          if (active && moveFocus) tab.focus();
        });

        var activeTab = tabs[nextIndex];
        var targetPanelId =
          activeTab &&
          (activeTab.getAttribute('aria-controls') ||
            activeTab.getAttribute('data-tab-target'));

        panels.forEach(function (panel, i) {
          var match = targetPanelId
            ? panel.id === targetPanelId ||
              panel.getAttribute('data-tab-panel') === targetPanelId
            : i === nextIndex;
          panel.hidden = !match;
        });
      }

      tabs.forEach(function (tab, index) {
        tab.addEventListener('click', function () {
          selectTab(index, false);
        });

        tab.addEventListener('keydown', function (event) {
          var targetIndex = -1;
          if (event.key === 'ArrowRight') {
            targetIndex = (index + 1) % tabs.length;
          } else if (event.key === 'ArrowLeft') {
            targetIndex = (index - 1 + tabs.length) % tabs.length;
          } else if (event.key === 'Home') {
            targetIndex = 0;
          } else if (event.key === 'End') {
            targetIndex = tabs.length - 1;
          }
          if (targetIndex >= 0) {
            event.preventDefault();
            selectTab(targetIndex, true);
          }
        });
      });

      var initialIndex = 0;
      for (var j = 0; j < tabs.length; j++) {
        if (
          tabs[j].getAttribute('aria-selected') === 'true' ||
          tabs[j].classList.contains('is-active')
        ) {
          initialIndex = j;
          break;
        }
      }
      selectTab(initialIndex, false);
    });
  }

  function initCodeCopy() {
    document.addEventListener('click', function (event) {
      var btn =
        event.target.closest &&
        event.target.closest('.code-copy, [data-copy]');
      if (!btn) return;

      var explicitText = btn.getAttribute('data-copy');
      var textToCopy = '';

      if (explicitText && explicitText !== 'auto') {
        var targetSelector = btn.getAttribute('data-copy-target');
        if (targetSelector) {
          var targetNode = document.querySelector(targetSelector);
          textToCopy = targetNode ? targetNode.textContent : explicitText;
        } else {
          textToCopy = explicitText;
        }
      } else {
        var container = btn.closest('.code-showcase, .codeblock');
        if (container) {
          var visiblePre =
            container.querySelector('.code-panel:not([hidden]) pre') ||
            container.querySelector('pre');
          if (visiblePre) {
            // Strip non-selectable shell prompt markers when copying CLI snippets
            var clone = visiblePre.cloneNode(true);
            clone.querySelectorAll('.prompt').forEach(function (p) {
              p.remove();
            });
            textToCopy = clone.textContent.trim();
          }
        }
      }

      if (!textToCopy || !navigator.clipboard) return;

      var originalLabel = btn.dataset.originalLabel || btn.textContent;
      btn.dataset.originalLabel = originalLabel;

      navigator.clipboard.writeText(textToCopy).then(
        function () {
          btn.textContent = '已复制';
          clearTimeout(Number(btn.dataset.copyTimer || 0));
          btn.dataset.copyTimer = String(
            setTimeout(function () {
              btn.textContent = originalLabel;
            }, 1600)
          );
        },
        function () {
          btn.textContent = '复制失败';
          setTimeout(function () {
            btn.textContent = originalLabel;
          }, 1600);
        }
      );
    });
  }

  /* ------------------------------------------------------------------------
     Bootstrap
     ------------------------------------------------------------------------ */
  function boot() {
    initNavigation();
    initSmoothAnchors();
    initCountUp();
    initInfinityEmblem();
    initIdeaMaps();
    initInteractiveStructures();
    initSystemRoute();
    initSectionHandoffs();
    initScrollTelemetry();
    initCodeTabs();
    initCodeCopy();
  }

  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', boot, { once: true });
  } else {
    boot();
  }
})();
