(function(){'use strict';
var tabs=Array.from(document.querySelectorAll('[role="tab"]'));
function activate(index,focus){tabs.forEach(function(tab,i){var active=i===index;tab.setAttribute('aria-selected',String(active));tab.tabIndex=active?0:-1;document.getElementById(tab.getAttribute('aria-controls')).hidden=!active;});if(focus)tabs[index].focus();}
tabs.forEach(function(tab,i){tab.addEventListener('click',function(){activate(i,false);});tab.addEventListener('keydown',function(e){var next=i;if(e.key==='ArrowRight')next=(i+1)%tabs.length;else if(e.key==='ArrowLeft')next=(i+tabs.length-1)%tabs.length;else if(e.key==='Home')next=0;else if(e.key==='End')next=tabs.length-1;else return;e.preventDefault();activate(next,true);});});
})();
