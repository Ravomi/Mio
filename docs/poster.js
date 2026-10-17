// Mio — poster edition: crop handles, leader lines, live clock, cat glitch.
(function () {
  'use strict';
  var NS = 'http://www.w3.org/2000/svg';

  function addHandles() {
    document.querySelectorAll('.crop').forEach(function (c) {
      if (c.querySelector(':scope > .hd')) return;
      ['tl', 'tm', 'tr', 'ml', 'mr', 'bl', 'bm', 'br'].forEach(function (p) {
        var i = document.createElement('i');
        i.className = 'hd hd-' + p;
        i.setAttribute('aria-hidden', 'true');
        c.appendChild(i);
      });
    });
  }

  function visible(el) {
    return el && el.offsetParent !== null && el.getClientRects().length > 0;
  }

  function rel(el, origin) {
    var r = el.getBoundingClientRect();
    return { l: r.left - origin.left, t: r.top - origin.top, r: r.right - origin.left, b: r.bottom - origin.top, w: r.width, h: r.height };
  }

  function addLine(svg, x1, y1, x2, y2, delay) {
    var ln = document.createElementNS(NS, 'line');
    ln.setAttribute('x1', x1.toFixed(1));
    ln.setAttribute('y1', y1.toFixed(1));
    ln.setAttribute('x2', x2.toFixed(1));
    ln.setAttribute('y2', y2.toFixed(1));
    var len = Math.hypot(x2 - x1, y2 - y1);
    ln.style.setProperty('--len', len.toFixed(0));
    ln.style.setProperty('--d', delay.toFixed(2) + 's');
    svg.appendChild(ln);
  }

  // data-fan="idA,idB": lines from the source's centre to the two nearest corners of each target frame.
  function drawLeaders() {
    var narrow = window.innerWidth < 760;
    document.querySelectorAll('[data-leaders]').forEach(function (scope) {
      var svg = scope.querySelector(':scope > svg.leaders');
      if (!svg) {
        svg = document.createElementNS(NS, 'svg');
        svg.setAttribute('class', 'leaders');
        svg.setAttribute('aria-hidden', 'true');
        scope.insertBefore(svg, scope.firstChild);
      }
      while (svg.firstChild) svg.removeChild(svg.firstChild);
      var o = scope.getBoundingClientRect();
      svg.setAttribute('viewBox', '0 0 ' + o.width.toFixed(0) + ' ' + o.height.toFixed(0));
      if (narrow && scope.hasAttribute('data-leaders-wide')) return;
      var n = 0;
      scope.querySelectorAll('[data-fan]').forEach(function (src) {
        if (!visible(src)) return;
        var s = rel(src, o);
        var sx = s.l + s.w / 2;
        var sy = s.t + s.h / 2;
        src.getAttribute('data-fan').split(',').forEach(function (id) {
          var t = document.getElementById(id.trim());
          if (!visible(t)) return;
          var r = rel(t, o);
          var cx = r.l + r.w / 2;
          var cy = r.t + r.h / 2;
          var dx = cx - sx;
          var dy = cy - sy;
          var pts;
          if (Math.abs(dx) / (r.w || 1) >= Math.abs(dy) / (r.h || 1)) {
            var x = dx > 0 ? r.l : r.r;
            pts = [[x, r.t], [x, r.b]];
          } else {
            var y = dy > 0 ? r.t : r.b;
            pts = [[r.l, y], [r.r, y]];
          }
          pts.forEach(function (p) {
            addLine(svg, sx, sy, p[0], p[1], 0.12 * n++);
          });
        });
      });
    });
  }

  function watchLeaders() {
    var scopes = document.querySelectorAll('[data-leaders]');
    if (!('IntersectionObserver' in window)) {
      scopes.forEach(function (s) { s.classList.add('lines-on'); });
      return;
    }
    var io = new IntersectionObserver(function (entries) {
      entries.forEach(function (e) {
        if (e.isIntersecting) {
          e.target.classList.add('lines-on');
          io.unobserve(e.target);
        }
      });
    }, { threshold: 0.2 });
    scopes.forEach(function (s) { io.observe(s); });
  }

  function liveClock() {
    var el = document.querySelector('.live-clock');
    if (!el) return;
    var pad = function (v) { return (v < 10 ? '0' : '') + v; };
    var tick = function () {
      var d = new Date();
      el.textContent = '(' + pad(d.getHours()) + ':' + pad(d.getMinutes()) + ':' + pad(d.getSeconds()) + ')';
    };
    tick();
    setInterval(tick, 1000);
  }

  function catGlitch() {
    if (window.matchMedia && window.matchMedia('(prefers-reduced-motion: reduce)').matches) return;
    var cats = document.querySelectorAll('.cat');
    if (!cats.length) return;
    setInterval(function () {
      cats.forEach(function (c) {
        c.classList.add('is-glitching');
        setTimeout(function () { c.classList.remove('is-glitching'); }, 180);
      });
    }, 4200);
  }

  var raf = 0;
  function schedule() {
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(drawLeaders);
  }

  document.addEventListener('DOMContentLoaded', function () {
    addHandles();
    drawLeaders();
    watchLeaders();
    liveClock();
    catGlitch();
    window.addEventListener('resize', schedule, { passive: true });
    window.addEventListener('load', schedule);
    if (document.fonts && document.fonts.ready) document.fonts.ready.then(schedule);
    if ('ResizeObserver' in window) {
      var ro = new ResizeObserver(schedule);
      document.querySelectorAll('[data-leaders]').forEach(function (s) { ro.observe(s); });
    }
  });
})();
