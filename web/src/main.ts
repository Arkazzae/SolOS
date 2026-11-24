// Sol in a browser: the app (ui/App.svelte) on the page's one element.

import './styles/sol.css';
import { mount } from 'svelte';
import App from './ui/App.svelte';

mount(App, { target: document.querySelector('#app')! });
