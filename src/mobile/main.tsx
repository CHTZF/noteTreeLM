import React from 'react'
import ReactDOM from 'react-dom/client'
import MobileApp from './MobileApp'

// Minimal global reset for mobile
const style = document.createElement('style')
style.textContent = `
  *, *::before, *::after { box-sizing: border-box; margin: 0; padding: 0; }
  body { background: #000; -webkit-tap-highlight-color: transparent; overscroll-behavior: none; }
  input, textarea { -webkit-appearance: none; }
  @keyframes blink { 50% { opacity: 0; } }
`
document.head.appendChild(style)

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <MobileApp />
  </React.StrictMode>
)
