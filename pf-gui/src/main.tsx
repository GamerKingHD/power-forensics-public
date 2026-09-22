import React from 'react'
import { createRoot } from 'react-dom/client'
import 'uplot/dist/uPlot.min.css'
import './styles/app.css'
import { App } from './app/App'

const root = document.getElementById('root')
if (!root) throw new Error('root element missing')

createRoot(root).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
)
