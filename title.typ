// #let template(doc) = {
//   // context if state("bundle").get() != true {
//   context if query(<sub-document-begin>).len() == 0 {
//     set document(title: [Title])
//   }
//   set text(10pt)
//   // set document(title: [Title])
//   // set ...
//   // set ...
//   // set ...
//   // show ...
//   // show ...
//   // show ...
//   doc
// }
// #show: template
#context if true { set document(title: [Title]) }
// #set document(title: [Title])
Text
// #context document.title
#set text(10pt)
Text
